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

/// 控制条窗口(逻辑像素)。宽度固定,避免时长刷新带动按钮;
/// 高度在紧凑态与展开上限之间跟随内容,换行后的按钮仍在窗口内。
const CONTROL_WIDTH: f64 = 420.0;
const CONTROL_HEIGHT: f64 = 64.0;
const CONTROL_HEIGHT_EXPANDED: f64 = 248.0;
/// 控制条与录制区域/显示器边缘的间距(逻辑像素)。
const CONTROL_MARGIN: f64 = 12.0;
/// 区域边框厚度(逻辑像素)。画在捕获矩形之外,不进入成片。
const BORDER_LOGICAL: f64 = 4.0;
/// 让给控制条的高度:紧凑条、一行说明,再加录制标注那一行。
/// 160 装得下换行后的控制行和 28px 工具行,也留得下 180px 高的显示器让位。
const CONTROL_RESERVED_LOGICAL: f64 = CONTROL_HEIGHT + 40.0 + 56.0;
/// 预览分片读取上限,避免一次 IPC 塞进整段 30 分钟成片。
const PREVIEW_CHUNK_BYTES: usize = 192 * 1024;
const BORDER_LABELS: [&str; 4] = [
    "record-border-top",
    "record-border-right",
    "record-border-bottom",
    "record-border-left",
];
/// 与 `.record-overlay-notice` 的 `bottom: 16px` 一致(标注层 CSS 像素)。
#[cfg(test)]
const NOTICE_BOTTOM_INSET: f64 = 16.0;
/// 与标注层 `NOTICE_GAP` 一致:再近就视为挡住控制条。
#[cfg(test)]
const NOTICE_CONTROL_GAP: f64 = 8.0;
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
    /// 最近一次放置控制条使用的逻辑尺寸;状态里的矩形与窗口一致。
    control_width: f64,
    control_height: f64,
    /// 让出边框/控制条之后真正抓取的矩形。录制中控制条不得再进入它。
    capture: Option<RecordRegion>,
    /// 控制条最近一次的屏幕物理矩形。
    control_screen: Option<ScreenRect>,
    /// 停止后待播放的临时成片。保存前不进入保存目录。
    preview: Option<super::RecordingOutput>,
    /// R4/R5:chrome 最近一次规划采用的确认矩形(与 `capture` 配对)。
    /// 更新区域时优先用它比对,避免与前端状态相位错位。
    confirmed_region: Option<RecordRegion>,
}

static HUD: Mutex<HudRuntime> = Mutex::new(HudRuntime {
    region: None,
    monitor: None,
    interactive: false,
    paused_for_draw: false,
    control_width: CONTROL_WIDTH,
    control_height: CONTROL_HEIGHT,
    capture: None,
    control_screen: None,
    preview: None,
    confirmed_region: None,
});

fn hud_lock() -> MutexGuard<'static, HudRuntime> {
    HUD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HudRegion {
    /// R4/R5:确认矩形相对录制监视器的原点(物理像素)。前端拖框以此为
    /// 基准计算新确认矩形,并回提 `update_recording_region`。
    pub x: u32,
    pub y: u32,
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
    /// 控制条在标注层坐标系中的矩形(CSS 像素)。无区域上下文时省略。
    /// 区域外放置时坐标可以越出标注层。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_frame: Option<HudControlFrame>,
    /// 单次录制上限(毫秒),控制条据此显示 已录/上限。
    pub limit_ms: u64,
    /// 停止后可播放的临时成片。保存前只有这一份,不在保存目录里。
    pub preview: Option<RecordingPreviewInfo>,
    /// 当前格式实际会使用的帧率档(录制中与会话一致)。
    pub fps: u32,
    /// 就绪态直给的格式:状态里尚无 status 时控制条也显示格式徽标。
    /// 与 `settings::current_recording().format` 同步;有 status 时用 status.format。
    pub format: RecordFormat,
}

/// 停止后先播放的成片。`temp_path` 只指向临时文件。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingPreviewInfo {
    pub temp_path: String,
    pub format: RecordFormat,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub fps: u32,
    pub auto_stopped: bool,
}

/// 控制条相对标注层左上角的矩形,单位是 CSS 像素。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HudControlFrame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
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
    /// 已停止并可以播放,还没有写入保存目录。
    Preview,
}

/// 打开 HUD:定位控制条、区域边框与标注层并开始广播状态。
pub fn open(app: &AppHandle, region: RecordRegion, monitor: MonitorGeom) {
    {
        let mut hud = hud_lock();
        hud.region = Some(region);
        hud.monitor = Some(monitor.clone());
        hud.interactive = false;
        hud.paused_for_draw = false;
        hud.preview = None;
    }
    apply_chrome(app, region, &monitor);
    broadcast_open(app);
    broadcast_state(app);
}

fn apply_chrome(app: &AppHandle, region: RecordRegion, monitor: &MonitorGeom) {
    let format = settings::current_recording(app).format;
    let spec = chrome_spec_for(monitor, format);
    let Some(plan) = plan_recording_chrome(region, monitor, spec).ok() else {
        return;
    };
    let capabilities = capabilities();
    {
        let mut hud = hud_lock();
        hud.region = Some(region);
        hud.monitor = Some(monitor.clone());
        hud.capture = Some(plan.capture);
        hud.control_screen = Some(plan.control);
        hud.control_width = CONTROL_WIDTH;
        hud.control_height = CONTROL_HEIGHT;
        hud.confirmed_region = Some(region);
    }
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
        place_screen_rect(&control, plan.control);
        let _ = control.show();
    }
    sync_borders(app, &plan.borders);
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
        // R4/R5:区域框(确认矩形)内的拖动是移动录制框;标注模式由控制条
        // 进入,两套输入互斥。就绪态且未进入标注模式时接收鼠标,
        // 让前端可以拖框;录制中/标注时保持穿透,避免遮挡或抢输入。
        let draggable = matches!(state_phase(app), Some(RecordingPhase::Ready))
            && !hud_lock().interactive;
        let _ = overlay.set_ignore_cursor_events(!draggable);
        if capabilities.live_overlay {
            let _ = overlay.show();
        } else {
            let _ = overlay.hide();
        }
    }
}

/// 当前会话的对外阶段:HUD 用它决定边框/overlay 是否可拖动。
fn state_phase(app: &AppHandle) -> Option<RecordingPhase> {
    session::with_recording(app, |recording| recording.status().phase)
}

/// R4/R5:重放区域拖动——按已经算好的 chrome plan 同步边框、overlay 与
/// 控制条位置;`plan`/`monitor` 与写入会话槽位的是同一次计算,保证
/// 边框跟实际 crop 矩形始终一致。
fn replay_chrome(app: &AppHandle, region: RecordRegion, monitor: &MonitorGeom, plan: &ChromePlan) {
    {
        let mut hud = hud_lock();
        // 确认矩形跟随拖框:状态里的 region 始终是最新显示框,前端拖框
        // 基准与 overlay 尺寸都取自它。
        hud.region = Some(region);
        hud.capture = Some(plan.capture);
        hud.control_screen = Some(plan.control);
        hud.confirmed_region = Some(region);
    }
    if let Some(control) = app.get_webview_window(CONTROL) {
        let _ = place_screen_rect(&control, plan.control);
    }
    sync_borders(app, &plan.borders);
    if let Some(overlay) = app.get_webview_window(OVERLAY) {
        let _ = overlay.set_size(Size::Physical(PhysicalSize {
            width: region.width.max(1),
            height: region.height.max(1),
        }));
        let _ = overlay.set_position(Position::Physical(PhysicalPosition {
            x: monitor.physical_x + region.x as i32,
            y: monitor.physical_y + region.y as i32,
        }));
    }
    broadcast_state(app);
}

fn place_screen_rect(window: &WebviewWindow, rect: ScreenRect) {
    let _ = window.set_size(Size::Physical(PhysicalSize {
        width: u32::try_from(rect.width).unwrap_or(1).max(1),
        height: u32::try_from(rect.height).unwrap_or(1).max(1),
    }));
    let _ = window.set_position(Position::Physical(PhysicalPosition {
        x: rect.x,
        y: rect.y,
    }));
}

fn sync_borders(app: &AppHandle, borders: &[Option<ScreenRect>; 4]) {
    let protection = capabilities().capture_protection;
    for (label, rect) in BORDER_LABELS.iter().zip(borders.iter()) {
        let Some(rect) = rect.filter(|rect| rect.width > 0 && rect.height > 0) else {
            if let Some(window) = app.get_webview_window(label) {
                let _ = window.hide();
            }
            continue;
        };
        let Ok(window) = ensure(app, label, "record-overlay&chrome=border", 8.0, 8.0, false) else {
            continue;
        };
        let _ = window.set_content_protected(protection);
        // R4/R5:边框窗可命中——把鼠标事件交给前端拖框逻辑(就绪态调整,
        // 录制中缩放句柄禁用,拖边=移动);前端只承担命中与提交,
        // 位置写回仍由 Rust 经 `update_recording_region` 完成。
        let _ = window.set_ignore_cursor_events(false);
        place_screen_rect(&window, rect);
        let _ = window.show();
    }
}

fn hide_borders(app: &AppHandle) {
    for label in BORDER_LABELS {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.hide();
        }
    }
}

/// 收起 HUD(保存成功/丢弃/没有活动会话且无待处理产物时)。
pub fn close(app: &AppHandle) {
    let preview = {
        let mut hud = hud_lock();
        hud.interactive = false;
        hud.paused_for_draw = false;
        hud.capture = None;
        hud.control_screen = None;
        hud.control_height = CONTROL_HEIGHT;
        hud.confirmed_region = None;
        hud.preview.take()
    };
    if let Some(output) = preview {
        // 关掉控制条却没保存:临时成片不能留在保存目录,也不继续占着临时文件。
        save::discard_recording(&output);
    }
    hide_borders(app);
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
    if label == OVERLAY || label.starts_with("record-border") {
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
    let capture = hud_lock().capture;
    let rect = if let Some(capture) = capture {
        control_rect_outside_capture(capture, monitor, size)
    } else {
        let (x, y) = control_origin(region, monitor, size, true);
        let scale = monitor.scale.max(f64::EPSILON);
        ScreenRect {
            x,
            y,
            width: physical_px(size.width, scale),
            height: physical_px(size.height, scale),
        }
    };
    {
        let mut hud = hud_lock();
        hud.control_width = size.width;
        hud.control_height = size.height;
        hud.control_screen = Some(rect);
    }
    place_screen_rect(control, rect);
}

/// 录制进行中控制条只能停在捕获矩形外面。外面不够时把窗口高度收到让出的带子里。
fn control_rect_outside_capture(
    capture: RecordRegion,
    monitor: &MonitorGeom,
    size: LogicalSize<f64>,
) -> ScreenRect {
    let scale = monitor.scale.max(f64::EPSILON);
    let mon_w = monitor.physical_width as i32;
    let mon_h = monitor.physical_height as i32;
    let cap_x = capture.x as i32;
    let cap_y = capture.y as i32;
    let cap_w = capture.width as i32;
    let cap_h = capture.height as i32;
    let margin = physical_px(CONTROL_MARGIN, scale);
    let width = physical_px(size.width, scale).min(mon_w.max(1));
    let mut height = physical_px(size.height, scale);
    let bottom_space = mon_h - (cap_y + cap_h);
    let top_space = cap_y;
    let y = if bottom_space >= height + margin {
        cap_y + cap_h + margin
    } else if top_space >= height + margin {
        cap_y - height - margin
    } else if bottom_space > margin {
        height = (bottom_space - margin).max(1);
        cap_y + cap_h + margin
    } else if top_space > margin {
        height = (top_space - margin).max(1);
        cap_y - height - margin
    } else {
        (mon_h - height).max(0)
    };
    let x = (cap_x + (cap_w - width) / 2).clamp(0, (mon_w - width).max(0));
    let y = y.clamp(0, (mon_h - height).max(0));
    ScreenRect {
        x: monitor.physical_x + x,
        y: monitor.physical_y + y,
        width,
        height,
    }
}

#[cfg(test)]
fn overlay_css_size(region: RecordRegion, monitor: &MonitorGeom) -> (f64, f64) {
    let scale = monitor.scale.max(f64::EPSILON);
    (region.width as f64 / scale, region.height as f64 / scale)
}

/// 控制条在标注层坐标系中的矩形。`prefer_outside` 与 [`place_control`] 一致时,
/// 全屏或贴住显示器上下沿会落在区域内侧底边。
fn control_frame_in_region(
    region: RecordRegion,
    monitor: &MonitorGeom,
    size: LogicalSize<f64>,
    prefer_outside: bool,
) -> HudControlFrame {
    let (px, py) = control_origin(region, monitor, size, prefer_outside);
    let scale = monitor.scale.max(f64::EPSILON);
    let region_left = monitor.physical_x + region.x as i32;
    let region_top = monitor.physical_y + region.y as i32;
    HudControlFrame {
        x: (px - region_left) as f64 / scale,
        y: (py - region_top) as f64 / scale,
        width: size.width,
        height: size.height,
    }
}

/// 贴在区域底部、水平居中的提示矩形。高度由调用方按实际文案给出。
#[cfg(test)]
fn bottom_notice_frame(
    region: RecordRegion,
    monitor: &MonitorGeom,
    notice_width: f64,
    notice_height: f64,
) -> HudControlFrame {
    let (region_width, region_height) = overlay_css_size(region, monitor);
    let width = notice_width.max(0.0).min(region_width.max(0.0));
    let height = notice_height.max(0.0);
    HudControlFrame {
        x: (region_width - width) / 2.0,
        y: region_height - NOTICE_BOTTOM_INSET - height,
        width,
        height,
    }
}

#[cfg(test)]
fn rects_conflict(a: HudControlFrame, b: HudControlFrame, gap: f64) -> bool {
    a.x < b.x + b.width + gap
        && a.x + a.width > b.x - gap
        && a.y < b.y + b.height + gap
        && a.y + a.height > b.y - gap
}

/// 底部提示与控制条相交时不留在区域上(改由控制卡片显示),因此可见矩形为空。
#[cfg(test)]
fn visible_region_notice(
    notice: HudControlFrame,
    control: HudControlFrame,
) -> Option<HudControlFrame> {
    if rects_conflict(notice, control, NOTICE_CONTROL_GAP) {
        None
    } else {
        Some(notice)
    }
}

/// 屏幕物理像素矩形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl ScreenRect {
    fn intersects(self, other: Self) -> bool {
        self.width > 0
            && self.height > 0
            && other.width > 0
            && other.height > 0
            && self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
}

/// 边框和控制条的放置参数,单位是物理像素。
#[derive(Debug, Clone, Copy)]
pub struct ChromeSpec {
    pub control_width: i32,
    pub control_height: i32,
    pub control_margin: i32,
    pub border: i32,
    pub capture_protection: bool,
    pub even: bool,
}

/// 捕获矩形、四条边框和控制条。边框顺序是上、右、下、左。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromePlan {
    pub capture: RecordRegion,
    pub borders: [Option<ScreenRect>; 4],
    pub control: ScreenRect,
    /// 捕获矩形比确认矩形小,边框或控制条占用了让出的位置。
    pub yielded: bool,
}

fn physical_px(logical: f64, scale: f64) -> i32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    ((logical * scale).round() as i32).max(1)
}

/// 与会话抓帧使用同一套尺寸,避免边框和控制条跟成片错位。
pub fn chrome_spec_for(monitor: &MonitorGeom, format: RecordFormat) -> ChromeSpec {
    let scale = monitor.scale;
    ChromeSpec {
        control_width: physical_px(CONTROL_WIDTH, scale),
        control_height: physical_px(CONTROL_RESERVED_LOGICAL, scale),
        control_margin: physical_px(CONTROL_MARGIN, scale),
        border: physical_px(BORDER_LOGICAL, scale),
        capture_protection: capabilities().capture_protection,
        even: format == RecordFormat::Mp4,
    }
}

pub fn annotation_inset(confirmed: RecordRegion, capture: RecordRegion) -> (i32, i32) {
    (
        capture.x as i32 - confirmed.x as i32,
        capture.y as i32 - confirmed.y as i32,
    )
}

/// 边框画在捕获矩形外面。四边都贴住显示器且没有内容保护时,捕获矩形向内让出边框。
/// 控制条放不下时,从矩形一边让出控制条高度。让出后的尺寸就是成片尺寸。
pub fn plan_recording_chrome(
    region: RecordRegion,
    monitor: &MonitorGeom,
    spec: ChromeSpec,
) -> Result<ChromePlan, ()> {
    let mon_w = i32::try_from(monitor.physical_width).unwrap_or(i32::MAX);
    let mon_h = i32::try_from(monitor.physical_height).unwrap_or(i32::MAX);
    if mon_w <= 0 || mon_h <= 0 {
        return Err(());
    }
    let mut x = i32::try_from(region.x).map_err(|_| ())?;
    let mut y = i32::try_from(region.y).map_err(|_| ())?;
    let mut w = i32::try_from(region.width).map_err(|_| ())?;
    let mut h = i32::try_from(region.height).map_err(|_| ())?;
    if x < 0
        || y < 0
        || w <= 0
        || h <= 0
        || x.saturating_add(w) > mon_w
        || y.saturating_add(h) > mon_h
    {
        return Err(());
    }
    let border = spec.border.max(1);
    let margin = spec.control_margin.max(0);
    let control_w = spec.control_width.clamp(1, mon_w);
    let control_h = spec.control_height.max(1);
    let gap = margin.max(border);
    let band = control_h.saturating_add(gap);
    let min_edge = if spec.even { 2 } else { 1 };
    let outside = |x: i32, y: i32, w: i32, h: i32| [x, y, mon_w - (x + w), mon_h - (y + h)];
    let confirmed_space = outside(x, y, w, h);
    let all_flush = confirmed_space.iter().all(|side| *side < border);
    let mut overlap_border = false;
    if all_flush {
        if spec.capture_protection {
            overlap_border = true;
        } else if w < border * 2 + min_edge || h < border * 2 + min_edge {
            return Err(());
        } else {
            x += border;
            y += border;
            w -= border * 2;
            h -= border * 2;
        }
    }
    let bottom_space = mon_h - (y + h);
    let top_space = y;
    if bottom_space >= band || top_space >= band {
        // 控制条放在捕获矩形外面。
    } else if h >= band + min_edge {
        h -= band;
    } else {
        return Err(());
    }
    if spec.even {
        let even_w = w & !1;
        let even_h = h & !1;
        if even_w < min_edge || even_h < min_edge {
            return Err(());
        }
        w = even_w;
        h = even_h;
    }
    if w < min_edge || h < min_edge {
        return Err(());
    }
    let bottom_space = mon_h - (y + h);
    let top_space = y;
    let control_y = if bottom_space >= control_h + gap {
        y + h + gap
    } else if top_space >= control_h + gap {
        y - control_h - gap
    } else {
        return Err(());
    };
    let control_x = (x + (w - control_w) / 2).clamp(0, (mon_w - control_w).max(0));
    let control_y = control_y.clamp(0, (mon_h - control_h).max(0));
    let screen = |local_x: i32, local_y: i32, width: i32, height: i32| ScreenRect {
        x: monitor.physical_x.saturating_add(local_x),
        y: monitor.physical_y.saturating_add(local_y),
        width,
        height,
    };
    let capture_screen = screen(x, y, w, h);
    let control = screen(control_x, control_y, control_w, control_h);
    if control.intersects(capture_screen) {
        return Err(());
    }
    let space = outside(x, y, w, h);
    let mut borders = [None, None, None, None];
    let push = |slot: &mut Option<ScreenRect>, rect: ScreenRect| {
        if rect.width > 0 && rect.height > 0 && !rect.intersects(control) {
            *slot = Some(rect);
        }
    };
    if space[1] >= border {
        push(&mut borders[0], screen(x, y - border, w, border));
    } else if overlap_border {
        push(&mut borders[0], screen(x, y, w, border.min(h)));
    }
    if space[2] >= border {
        push(&mut borders[1], screen(x + w, y, border, h));
    } else if overlap_border {
        push(
            &mut borders[1],
            screen(x + w - border.min(w), y, border.min(w), h),
        );
    }
    if space[3] >= border {
        push(&mut borders[2], screen(x, y + h, w, border));
    } else if overlap_border {
        push(
            &mut borders[2],
            screen(x, y + h - border.min(h), w, border.min(h)),
        );
    }
    if space[0] >= border {
        push(&mut borders[3], screen(x - border, y, border, h));
    } else if overlap_border {
        push(&mut borders[3], screen(x, y, border.min(w), h));
    }
    if !overlap_border
        && borders
            .iter()
            .flatten()
            .any(|border_rect| border_rect.intersects(capture_screen))
    {
        return Err(());
    }
    let capture = RecordRegion::new(
        u32::try_from(x).unwrap_or(0),
        u32::try_from(y).unwrap_or(0),
        u32::try_from(w).unwrap_or(0),
        u32::try_from(h).unwrap_or(0),
    );
    let yielded = capture.x != region.x
        || capture.y != region.y
        || capture.width != region.width
        || capture.height != region.height;
    Ok(ChromePlan {
        capture,
        borders,
        control,
        yielded,
    })
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
    let (interactive, context, control_size, control_screen, parked_preview) = {
        let hud = hud_lock();
        (
            hud.interactive,
            hud.region.zip(hud.monitor.clone()),
            LogicalSize {
                width: hud.control_width,
                height: hud.control_height,
            },
            hud.control_screen,
            hud.preview.clone(),
        )
    };
    let status = session::with_recording(app, |recording| recording.status());
    // R4/R5:overlay 的命中态跟阶段走——就绪/倒计时或标注模式下接收输入
    // (拖框/绘制),进入录制或结束后归还穿透,避免盖住区域内的鼠标操作。
    // 倒计时→录制由 worker 内部推进,这里借状态轮询收敛命中态。
    if let Some(overlay) = app.get_webview_window(OVERLAY) {
        let draggable = interactive
            || matches!(
                status.as_ref().map(|status| status.phase),
                Some(RecordingPhase::Ready) | Some(RecordingPhase::Countdown)
            );
        let _ = overlay.set_ignore_cursor_events(!draggable);
    }
    let live_preview = parked_preview
        .clone()
        .or_else(|| {
            session::with_recording(app, |recording| recording.finished_output()).flatten()
        });
    let recording_settings = settings::current_recording(app);
    let fps = status
        .as_ref()
        .map(|status| status.fps)
        .or_else(|| live_preview.as_ref().map(|output| output.fps))
        .unwrap_or_else(|| {
            super::resolve_recording_fps(recording_settings.format, recording_settings.fps)
        });
    RecordingHudState {
        status,
        pending: save::pending_recordings()
            .iter()
            .map(pending_info)
            .collect(),
        capabilities: capabilities(),
        interactive,
        has_context: context.is_some(),
        region: context.as_ref().map(|(region, monitor)| HudRegion {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
            scale: monitor.scale,
        }),
        control_frame: context.as_ref().and_then(|(region, monitor)| {
            control_screen
                .map(|rect| control_frame_from_screen(*region, monitor, rect))
                .or_else(|| {
                    Some(control_frame_in_region(
                        *region,
                        monitor,
                        control_size,
                        true,
                    ))
                })
        }),
        limit_ms: MAX_RECORDING_MS,
        preview: live_preview.as_ref().map(preview_info),
        fps,
        format: recording_settings.format,
    }
}

fn control_frame_from_screen(
    region: RecordRegion,
    monitor: &MonitorGeom,
    rect: ScreenRect,
) -> HudControlFrame {
    let scale = monitor.scale.max(f64::EPSILON);
    let left = monitor.physical_x + region.x as i32;
    let top = monitor.physical_y + region.y as i32;
    HudControlFrame {
        x: f64::from(rect.x - left) / scale,
        y: f64::from(rect.y - top) / scale,
        width: f64::from(rect.width) / scale,
        height: f64::from(rect.height) / scale,
    }
}

fn preview_info(output: &super::RecordingOutput) -> RecordingPreviewInfo {
    RecordingPreviewInfo {
        temp_path: output.temp_path.to_string_lossy().into_owned(),
        format: output.format,
        width: output.width,
        height: output.height,
        duration_ms: output.duration_ms,
        fps: output.fps,
        auto_stopped: output.auto_stopped,
    }
}

/// 控制条轮询端点:会话状态 + 待处理产物 + 能力说明。
#[tauri::command]
pub fn get_recording_hud_state(app: AppHandle) -> RecordingHudState {
    state_snapshot(&app)
}

/// 暂停/继续/开始控制。开始在无活动会话时按上次区域启动;已有活动会话时:
/// 就绪态 → 倒计时(`begin`),其它阶段幂等返回当前状态(防止重复开始)。
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
        "start" => {
            let existing = session::with_recording(&app, |recording| recording.begin());
            match existing {
                Some(Ok(status)) => Ok(status),
                Some(Err(error)) => Err(error.user_message()),
                // 无会话:退到「重新录制」路径。
                None => start_from_hud(&app),
            }
        }
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
    // 与选区确认入口一致:重录也走就绪/倒计时,而不是立即开录。
    let recording = RecordingSession::start_ready(region, config, MonitorSource::new(monitor.clone()))
        .map_err(|error| error.user_message())?;
    if !session::install_recording(app, recording) {
        return Err(i18n::t("toast.recording_busy"));
    }
    {
        let mut hud = hud_lock();
        hud.interactive = false;
        hud.paused_for_draw = false;
        hud.preview = None;
    }
    apply_chrome(app, region, &monitor);
    // 重新开始:标注层复位后按新会话的标注(空)重新初始化,旧会话残留的
    // 绘制内容不会继续显示或被误并入新录制。
    reset_overlay(app);
    session::refresh_tray_menu(app);
    broadcast_state(app);
    session::with_recording(app, |recording| recording.status())
        .ok_or_else(|| RecordError::NotRunning.user_message())
}

/// 停止并先播放(控制条「停止」/托盘停止/自动停止后的收尾)。
/// 不打开保存对话框,保存目录里也不会出现成品。
/// 就绪/倒计时内调用按取消处理:直接收尾、无产物、不进入预览。
#[tauri::command]
pub async fn stop_recording_from_hud(app: AppHandle) -> RecordingStopOutcome {
    let Some(recording) = session::take_recording_session(&app) else {
        if hud_lock().preview.is_some() {
            return RecordingStopOutcome::Preview;
        }
        return RecordingStopOutcome::Empty;
    };
    let pre_phase = recording.status().phase;
    if matches!(pre_phase, RecordingPhase::Ready | RecordingPhase::Countdown) {
        // 就绪/倒计时取消:不产帧 → stop 必走 Empty;这里只负责干净收尾
        // (Drop 已删临时文件),收 HUD 并保持无 pending 残留。
        recording.cancel();
        let _ = tauri::async_runtime::spawn_blocking(move || drop(recording)).await;
        close(&app);
        session::refresh_tray_menu(&app);
        return RecordingStopOutcome::Discarded;
    }
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
    present_preview(&app, output);
    RecordingStopOutcome::Preview
}

/// 把刚停下来的临时成片留在控制条里播放。保存前不写入保存目录。
pub fn present_preview(app: &AppHandle, output: super::RecordingOutput) {
    if output.auto_stopped {
        ui::show_toast_key(app, "toast.recording_auto_stopped");
    }
    if let Some(interrupted) = output.interrupted.as_deref() {
        ui::show_toast(app, interrupted);
    }
    {
        let mut hud = hud_lock();
        hud.preview = Some(output);
        hud.capture = None;
        hud.confirmed_region = None;
        hud.interactive = false;
        hud.paused_for_draw = false;
    }
    hide_borders(app);
    if let Some(overlay) = app.get_webview_window(OVERLAY) {
        let _ = overlay.hide();
    }
    let context = {
        let hud = hud_lock();
        hud.region.zip(hud.monitor.clone())
    };
    let capabilities = capabilities();
    if let Some((region, monitor)) = context {
        if let Ok(control) = ensure(
            app,
            CONTROL,
            "record-control",
            CONTROL_WIDTH,
            CONTROL_HEIGHT,
            true,
        ) {
            let _ = control.set_content_protected(capabilities.capture_protection);
            place_control(
                &control,
                region,
                &monitor,
                LogicalSize {
                    width: CONTROL_WIDTH,
                    height: CONTROL_HEIGHT,
                },
            );
            let _ = control.show();
            let _ = control.emit(EVENT_OPEN, ());
        }
    }
    session::refresh_tray_menu(app);
    broadcast_state(app);
}

fn take_preview_output(app: &AppHandle) -> Option<super::RecordingOutput> {
    if let Some(output) = hud_lock().preview.take() {
        return Some(output);
    }
    let recording = session::take_recording_session(app)?;
    recording
        .finished_output()
        .or_else(|| recording.stop().ok())
}

fn preview_path_allowed(app: &AppHandle, requested: &std::path::Path) -> bool {
    if hud_lock()
        .preview
        .as_ref()
        .is_some_and(|output| output.temp_path == requested)
    {
        return true;
    }
    if session::with_recording(app, |recording| {
        recording
            .finished_output()
            .is_some_and(|output| output.temp_path == requested)
    })
    .unwrap_or(false)
    {
        return true;
    }
    save::pending_recordings()
        .iter()
        .any(|output| output.temp_path == requested)
}

/// 保存已经停下来的预览。取消对话框不删除临时文件,保存目录仍没有成品。
#[tauri::command]
pub async fn save_recording_preview(app: AppHandle) -> RecordingStopOutcome {
    let Some(output) = take_preview_output(&app) else {
        return RecordingStopOutcome::Empty;
    };
    let parent = app.get_webview_window(CONTROL);
    let result = save::save_recording_with_dialog(&app, parent.as_ref(), &output).await;
    match result {
        Ok(result) if result.saved => {
            let name = result
                .path
                .as_deref()
                .and_then(|path| std::path::Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("cropmark")
                .to_string();
            ui::show_toast_key_params(&app, "toast.saved", &[("name", &name)]);
            session::refresh_tray_menu(&app);
            if save::pending_recordings().is_empty() {
                close(&app);
            } else {
                broadcast_state(&app);
            }
            RecordingStopOutcome::Saved { name }
        }
        Ok(_) => {
            // 用户取消保存:成片仍只在临时文件里,可以继续播放或丢弃。
            present_preview(&app, output);
            RecordingStopOutcome::Cancelled
        }
        Err(message) => {
            save::keep_pending_recording(output);
            ui::show_toast(&app, &message);
            session::refresh_tray_menu(&app);
            broadcast_state(&app);
            RecordingStopOutcome::Failed {
                message,
                retryable: true,
            }
        }
    }
}

/// 丢弃这次预览:删除临时文件,保存目录里没有成品。
#[tauri::command]
pub fn discard_recording_preview(app: AppHandle) -> bool {
    let Some(output) = take_preview_output(&app) else {
        return false;
    };
    save::discard_recording(&output);
    ui::show_toast_key(&app, "toast.recording_discarded");
    session::refresh_tray_menu(&app);
    broadcast_state(&app);
    if save::pending_recordings().is_empty() {
        close(&app);
    }
    true
}

/// 按偏移读取预览临时文件的一块 base64。只能读当前预览或待处理录制。
#[tauri::command]
pub fn read_recording_preview_chunk(
    app: AppHandle,
    temp_path: String,
    offset: u64,
) -> Result<Option<String>, String> {
    let path = std::path::PathBuf::from(&temp_path);
    if !preview_path_allowed(&app, &path) {
        return Err(i18n::t("error.record.pending_missing"));
    }
    let mut file = std::fs::File::open(&path)
        .map_err(|error| i18n::tp("error.record.temp_io", &[("detail", &error.to_string())]))?;
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if offset >= length {
        return Ok(None);
    }
    let take = usize::try_from(length - offset)
        .unwrap_or(PREVIEW_CHUNK_BYTES)
        .min(PREVIEW_CHUNK_BYTES);
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| RecordError::TempIo(error.to_string()).user_message())?;
    let mut buffer = vec![0u8; take];
    file.read_exact(&mut buffer)
        .map_err(|error| RecordError::TempIo(error.to_string()).user_message())?;
    Ok(Some(STANDARD.encode(buffer)))
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
        // R4/R5:就绪态未标注时 overlay 保持可命中(拖框移动);标注模式
        // 本来就要接收输入,录制中退出标注后才回到穿透。
        let ready_move = matches!(state_phase(&app), Some(RecordingPhase::Ready));
        let _ = overlay.set_ignore_cursor_events(!interactive && !ready_move);
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

/// 控制条逻辑尺寸。宽度保持 [`CONTROL_WIDTH`];高度跟随内容测量值,
/// 不小于紧凑态,也不超过展开上限。非有限测量值回落到紧凑态。
fn control_window_size(content_height: f64) -> LogicalSize<f64> {
    LogicalSize {
        width: CONTROL_WIDTH,
        height: control_window_height(content_height),
    }
}

fn control_window_height(content_height: f64) -> f64 {
    if !content_height.is_finite() {
        return CONTROL_HEIGHT;
    }
    content_height.clamp(CONTROL_HEIGHT, CONTROL_HEIGHT_EXPANDED)
}

/// 控制条按内容增高(换行、说明或待保存列表)。`content_height` 为前端量到的
/// 逻辑像素高度;缺省时仍按展开标志在紧凑态与展开上限之间二选一。
#[tauri::command]
pub fn set_recording_hud_expanded(app: AppHandle, expanded: bool, content_height: Option<f64>) {
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
    let requested = content_height
        .filter(|height| height.is_finite())
        .unwrap_or(if expanded {
            CONTROL_HEIGHT_EXPANDED
        } else {
            CONTROL_HEIGHT
        });
    place_control(&control, region, &monitor, control_window_size(requested));
    // 增高后控制条可能从区域外回到内侧底边,标注层要按新矩形重新避让。
    broadcast_state(&app);
}

/// R4/R5:拖框提交的确认矩形目标(未钳制)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionTarget {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// 就绪态拖框钳制:确认矩形必须是显示器内有效正尺寸矩形,完全落在显示器内。
/// 宽/高上限取显示器与格式上限(MP4 3840/2160,WebP 16383)的较小者;
/// 下限取编码器最小边(MP4 偶数 2,其余 1)。钳制到边界而不是拒绝,
/// 避免拖到边缘后反弹抖动。
fn clamp_region_target(
    target: RegionTarget,
    monitor: &MonitorGeom,
    format: RecordFormat,
) -> Option<RecordRegion> {
    let mon_w = i32::try_from(monitor.physical_width).ok()?;
    let mon_h = i32::try_from(monitor.physical_height).ok()?;
    let min_edge: i32 = if format == RecordFormat::Mp4 { 2 } else { 1 };
    let (max_w, max_h) = match format {
        RecordFormat::Gif => (mon_w, mon_h),
        RecordFormat::Webp => (mon_w.min(16383), mon_h.min(16383)),
        RecordFormat::Mp4 => (mon_w.min(3840), mon_h.min(2160)),
    };
    let width = target.width.clamp(min_edge, max_w.max(min_edge));
    let height = target.height.clamp(min_edge, max_h.max(min_edge));
    let x = target.x.clamp(0, (mon_w - width).max(0));
    let y = target.y.clamp(0, (mon_h - height).max(0));
    Some(RecordRegion::new(
        u32::try_from(x).ok()?,
        u32::try_from(y).ok()?,
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    ))
}

/// R4/R5 拖框入口:就绪态接受移动与缩放(宽高经钳制到合法范围),
/// 录制中只接受移动(宽高沿用现有框,缩放被忽略——编码器宽高建构期定死)。
/// 其它阶段拒绝。成功后写会话槽位并重放 chrome;输入带显示器内钳制,
/// 因此不会产生越界 crop。
#[tauri::command]
pub fn update_recording_region(
    app: AppHandle,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> bool {
    let Some(monitor) = hud_lock().monitor.clone() else {
        return false;
    };
    let Some((phase, display, format, locked)) = session::with_recording(&app, |recording| {
        let status = recording.status();
        (
            status.phase,
            recording.display_region(),
            status.format,
            recording.capture_region(),
        )
    }) else {
        return false;
    };
    let spec = chrome_spec_for(&monitor, format);
    // 录制/暂停:宽高沿用现有框——编码器建构期定死,前端缩放句柄
    // 按平移提交时直接丢弃宽高分量;就绪/倒计时可改宽高。
    let target = match phase {
        RecordingPhase::Ready | RecordingPhase::Countdown => RegionTarget {
            x,
            y,
            width,
            height,
        },
        RecordingPhase::Recording | RecordingPhase::Paused => RegionTarget {
            x,
            y,
            width: display.width as i32,
            height: display.height as i32,
        },
        _ => return false,
    };
    let Some(clamped) = clamp_region_target(target, &monitor, format) else {
        return false;
    };
    if clamped == display {
        return true;
    }
    // 就绪态缩放可能让确认矩形在 chrome 让位后过小(例如竖着拉高控制条
    // 让位宽度不足):压缩回上一个可规划成功的尺寸,而不是整块拒绝。
    let mut plan = plan_recording_chrome(clamped, &monitor, spec);
    let mut candidate = clamped;
    if plan.is_err() && clamped.width > display.width && clamped.height > display.height {
        // 同时放大两个方向失败:退回只沿拖动方向扩展(单边放大更容易成功)。
        let narrow = clamp_region_target(
            RegionTarget {
                x: target.x,
                y: target.y,
                width: target.width.min(display.width as i32),
                height: target.height.min(display.height as i32),
            },
            &monitor,
            format,
        );
        if let Some(fallback) = narrow {
            if let Ok(trial) = plan_recording_chrome(fallback, &monitor, spec) {
                candidate = fallback;
                plan = Ok(trial);
            }
        }
    }
    let Ok(plan) = plan else {
        return false;
    };
    // 录制中槽位捕获矩形的宽高必须是编码器锁死的尺寸——重放出的 plan
    // 可能因边框让位差异给出新宽高,但 worker 在编码器打开后只取原点。
    // 显式保持编码器矩形宽高,避免 status 宽高与成片不一致。
    let slot_capture = if matches!(phase, RecordingPhase::Recording | RecordingPhase::Paused) {
        RecordRegion::new(plan.capture.x, plan.capture.y, locked.width, locked.height)
    } else {
        plan.capture
    };
    if session::with_recording(&app, |recording| {
        recording.update_region(candidate, slot_capture);
    })
    .is_none()
    {
        return false;
    }
    replay_chrome(&app, candidate, &monitor, &plan);
    true
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
    fn control_window_size_keeps_width_and_grows_height_with_content() {
        let compact = control_window_size(1.0);
        assert_eq!(compact.width, CONTROL_WIDTH);
        assert_eq!(compact.height, CONTROL_HEIGHT);
        let wrapped = control_window_size(96.0);
        assert_eq!(wrapped.width, CONTROL_WIDTH);
        assert_eq!(wrapped.height, 96.0);
        let capped = control_window_size(CONTROL_HEIGHT_EXPANDED + 80.0);
        assert_eq!(capped.width, CONTROL_WIDTH);
        assert_eq!(capped.height, CONTROL_HEIGHT_EXPANDED);
        assert_eq!(control_window_size(f64::NAN).height, CONTROL_HEIGHT);
        assert_eq!(control_window_size(f64::INFINITY).height, CONTROL_HEIGHT);
    }

    #[test]
    fn wrapped_control_stays_outside_the_region_when_space_allows() {
        let size = control_window_size(120.0);
        let region = RecordRegion::new(100, 80, 900, 640);
        let (_, y) = control_origin(region, &monitor(), size, true);
        let region_bottom = 80 + 640;
        assert!(y >= region_bottom);
        assert!(y + size.height as i32 <= 1080);
        // 全屏区域外侧放不下:更高的控制条仍完整留在显示器内。
        let full = RecordRegion::new(0, 0, 1920, 1080);
        let (_, y) = control_origin(full, &monitor(), size, true);
        assert!(y >= 0 && y + size.height as i32 <= 1080);
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
        assert!(!i18n::t("record.hud.behind").contains("record.hud.behind"));
    }

    fn spec(protection: bool, even: bool) -> ChromeSpec {
        ChromeSpec {
            control_width: 420,
            control_height: 64,
            control_margin: 12,
            border: 4,
            capture_protection: protection,
            even,
        }
    }

    fn capture_screen(region: RecordRegion, monitor: &MonitorGeom) -> ScreenRect {
        ScreenRect {
            x: monitor.physical_x + region.x as i32,
            y: monitor.physical_y + region.y as i32,
            width: region.width as i32,
            height: region.height as i32,
        }
    }

    #[test]
    fn chrome_keeps_border_and_control_outside_when_the_margin_exists() {
        let screen = monitor();
        let region = RecordRegion::new(80, 60, 900, 640);
        let plan = plan_recording_chrome(region, &screen, spec(false, false)).expect("plan");
        assert!(!plan.yielded);
        assert_eq!((plan.capture.width, plan.capture.height), (900, 640));
        let capture = capture_screen(plan.capture, &screen);
        assert!(!plan.control.intersects(capture));
        assert!(plan.borders.iter().flatten().count() >= 3);
        for border in plan.borders.iter().flatten() {
            assert!(!border.intersects(capture));
            assert!(!border.intersects(plan.control));
        }
    }

    #[test]
    fn chrome_yields_when_fullscreen_has_no_outside_room() {
        let screen = monitor();
        let region = RecordRegion::new(0, 0, 1920, 1080);
        let unprotected = plan_recording_chrome(region, &screen, spec(false, true)).expect("plan");
        assert!(unprotected.yielded);
        let capture = capture_screen(unprotected.capture, &screen);
        assert!(!unprotected.control.intersects(capture));
        assert!(unprotected.borders.iter().flatten().count() == 4);
        for border in unprotected.borders.iter().flatten() {
            assert!(
                !border.intersects(capture),
                "unprotected border entered the capture"
            );
        }
        assert_eq!(unprotected.capture.width % 2, 0);
        assert_eq!(unprotected.capture.height % 2, 0);
        // 成片尺寸就是让出之后的捕获矩形。
        assert!(
            unprotected.capture.width < region.width || unprotected.capture.height < region.height
        );

        let protected = plan_recording_chrome(region, &screen, spec(true, true)).expect("plan");
        assert!(protected.yielded, "control bar still needs a yielded band");
        let protected_capture = capture_screen(protected.capture, &screen);
        assert!(!protected.control.intersects(protected_capture));
        assert!(protected.borders.iter().flatten().count() == 4);
        // 有内容保护时边框可以贴在矩形边缘,但控制条不能进入捕获矩形。
        assert!(protected.capture.width % 2 == 0 && protected.capture.height % 2 == 0);
    }

    #[test]
    fn chrome_refuses_a_region_too_small_for_the_control_bar() {
        let screen = MonitorGeom::from_physical("tiny", 0, 0, 40, 40, 1.0);
        let region = RecordRegion::new(0, 0, 40, 40);
        assert!(plan_recording_chrome(region, &screen, spec(false, false)).is_err());
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
            fps: 30,
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
                x: 6,
                y: 8,
                width: 4,
                height: 2,
                scale: 2.0,
            }),
            control_frame: None,
            limit_ms: MAX_RECORDING_MS,
            preview: None,
            fps: 30,
            format: RecordFormat::Mp4,
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
        assert!(json.get("controlFrame").is_none());
        assert_eq!(json["interactive"], false);
        assert_eq!(json["fps"], 30);
        assert!(json["preview"].is_null());
        assert_eq!(
            serde_json::to_value(RecordingStopOutcome::Preview).unwrap()["kind"],
            "preview"
        );
    }

    #[test]
    fn fullscreen_bottom_notice_does_not_intersect_the_control_bar() {
        let screen = monitor();
        let region = RecordRegion::new(0, 0, 1920, 1080);
        let control = control_frame_in_region(region, &screen, compact(), true);
        assert!(control.y >= 0.0 && control.y + control.height <= region.height as f64);
        let notice = bottom_notice_frame(region, &screen, 240.0, 32.0);
        assert!(
            rects_conflict(notice, control, 0.0),
            "bottom notice would cover the in-region control bar"
        );
        let visible = visible_region_notice(notice, control);
        assert!(
            visible.is_none(),
            "fullscreen notice moves into the control card"
        );
        assert!(!visible.is_some_and(|rect| rects_conflict(rect, control, 0.0)));

        let hidpi = MonitorGeom::from_physical("hidpi", 0, 0, 2560, 1440, 2.0);
        let full = RecordRegion::new(0, 0, 2560, 1440);
        let hidpi_control = control_frame_in_region(full, &hidpi, compact(), true);
        let hidpi_notice = bottom_notice_frame(full, &hidpi, 240.0, 32.0);
        assert!(visible_region_notice(hidpi_notice, hidpi_control).is_none());
    }

    #[test]
    fn bottom_notice_stays_when_the_control_bar_is_outside_the_region() {
        let screen = monitor();
        let region = RecordRegion::new(100, 100, 1200, 700);
        let control = control_frame_in_region(region, &screen, compact(), true);
        let notice = bottom_notice_frame(region, &screen, 240.0, 32.0);
        let visible = visible_region_notice(notice, control).expect("notice stays on the region");
        assert!(!rects_conflict(visible, control, 0.0));
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
    fn clamp_region_target_bounds_to_monitor_and_format_edges() {
        let screen = monitor();
        // GIF:下限 1,上限为显示器本身。
        let clamped = clamp_region_target(
            RegionTarget {
                x: -40,
                y: -20,
                width: 0,
                height: 0,
            },
            &screen,
            RecordFormat::Gif,
        )
        .expect("clamped");
        assert_eq!((clamped.x, clamped.y), (0, 0));
        assert_eq!((clamped.width, clamped.height), (1, 1));
        // MP4:下限偶数 2,上限 3840×2160;1920×1080 显示器取显示器边。
        let mp4 = clamp_region_target(
            RegionTarget {
                x: 0,
                y: 0,
                width: 9999,
                height: 9999,
            },
            &screen,
            RecordFormat::Mp4,
        )
        .expect("mp4");
        assert_eq!((mp4.width, mp4.height), (1920, 1080));
        // WebP:上限 16383,超过显示器仍按显示器钳。
        let webp = clamp_region_target(
            RegionTarget {
                x: 0,
                y: 0,
                width: 20000,
                height: 20000,
            },
            &MonitorGeom::from_physical("big", 0, 0, 20000, 20000, 1.0),
            RecordFormat::Webp,
        )
        .expect("webp");
        assert_eq!((webp.width, webp.height), (16383, 16383));
        // 显示器内拖动到边缘:原点被钳回,矩形仍完整。
        let edge = clamp_region_target(
            RegionTarget {
                x: 1900,
                y: 1070,
                width: 400,
                height: 300,
            },
            &screen,
            RecordFormat::Gif,
        )
        .expect("edge");
        assert_eq!((edge.x, edge.y), (1520, 780));
    }

    #[test]
    fn region_move_detects_origin_changes() {
        let current = RecordRegion::new(10, 20, 100, 80);
        assert_eq!(
            super::super::region_move(RecordRegion::new(10, 20, 100, 80), current),
            super::super::RegionMove::Steady
        );
        assert_eq!(
            super::super::region_move(RecordRegion::new(30, 20, 100, 80), current),
            super::super::RegionMove::Moved { x: 30, y: 20 }
        );
        // 录制中忽略宽高:即便槽位宽高漂移,只按原点判定移动。
        assert_eq!(
            super::super::region_move(RecordRegion::new(10, 20, 640, 480), current),
            super::super::RegionMove::Steady
        );
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
