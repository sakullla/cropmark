use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use super::buffer::{crop_rgba, encode_png, rotate_frame_ccw, rotate_frame_cw, Frame};
use super::error::CaptureError;
use super::geometry::{
    crop_from_logical, monitor_at_physical, monitor_dest, monitor_key, stitch_views,
    virtual_canvas, CanvasFault, LogicalRect, MonitorGeom, RgbaView, VirtualCanvas,
    STITCH_BACKGROUND,
};
use super::hide::{
    grab_allowed, hide_not_presented_error, plan_delay, wait_compositor_presented,
    wait_until_hidden, HideWait, RecordedSurface, SurfaceKind,
};
use super::platform;
use super::ui::{self, DelayPayload, OverlayPayload, PreviewPayload};
use super::windows_list::ListedWindow;
use crate::annotate::{rasterize_lenient, transformed_all, Annotation, FrameTransform};
use crate::clipboard::{self, ClipboardGuard};
use crate::hotkeys::CaptureMode;
use crate::ocr::OcrDocument;

/// 全屏采集目标。热键与关闭多屏开关时保持指针所在屏。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FullscreenTarget {
    Pointer,
    Monitor(String),
    All,
}

pub fn effective_fullscreen_target(
    multi_monitor: bool,
    target: FullscreenTarget,
) -> FullscreenTarget {
    if multi_monitor {
        target
    } else {
        FullscreenTarget::Pointer
    }
}

pub struct CaptureRuntime {
    inner: Mutex<Option<ActiveSession>>,
    last_error: Mutex<Option<CaptureError>>,
    /// R3:活动录制会话。选区入口启动;托盘(停止/保存)与后续录制 HUD 消费,
    /// 同一时间最多一个。
    recording: Mutex<Option<crate::record::RecordingSession>>,
}

impl Default for CaptureRuntime {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
            last_error: Mutex::new(None),
            recording: Mutex::new(None),
        }
    }
}

struct ActiveSession {
    mode: CaptureMode,
    busy: bool,
    delay_ms: u64,
    hide: HideWait,
    freeze: Option<Frame>,
    overlay: Option<OverlayPayload>,
    preview: Option<PreviewPayload>,
    monitor: Option<MonitorGeom>,
    windows: Vec<ListedWindow>,
    clipboard: ClipboardGuard,
    preview_opened: bool,
    file_written: bool,
    cancelled: bool,
    frame_deadline: Option<Instant>,
    started_at: Instant,
    /// 会话代际(ADR-16):旧壳结果携带自己的代际,不符则丢弃,不作用于新会话。
    generation: u64,
    /// 取消受理时刻(Cancelling 阶段计时;看门狗与超时提示用)。
    cancel_requested_at: Option<Instant>,
    /// 贴图再标注(R9):预览会话的回写目标 label;普通截取预览为 None,
    /// 会话被替换/关闭时随之失效。
    writeback: Option<String>,
    /// 选区「取字」打开预览后由前端消费一次,自动进入取字而不是静默退出。
    pending_ocr: bool,
    /// 选区「识别二维码」打开工作区后由前端消费一次,自动开始识别。
    pending_qr: bool,
    /// R6:预览会话级旋转/裁剪快照(帧由基准帧重放,标注与取字随步保存)。
    preview_transforms: PreviewTransforms,
    /// 全屏目标;非全屏会话保持指针屏。
    fullscreen_target: FullscreenTarget,
    /// 本次采集请求了指针但平台拿不到图像。完成时提示,不阻断输出。
    cursor_unavailable: bool,
}

impl ActiveSession {
    fn new(mode: CaptureMode, delay_ms: u64, now: Instant) -> Self {
        Self {
            mode,
            busy: true,
            delay_ms,
            hide: HideWait::record(Vec::new()),
            freeze: None,
            overlay: None,
            preview: None,
            monitor: None,
            windows: Vec::new(),
            clipboard: ClipboardGuard::default(),
            preview_opened: false,
            file_written: false,
            cancelled: false,
            frame_deadline: None,
            started_at: now,
            generation: next_session_generation(),
            cancel_requested_at: None,
            writeback: None,
            pending_ocr: false,
            pending_qr: false,
            preview_transforms: PreviewTransforms::default(),
            fullscreen_target: FullscreenTarget::Pointer,
            cursor_unavailable: false,
        }
    }

    /// Cancelling 阶段:取消请求已受理、清理尚未完成(会话仍占槽位)。
    /// 期间不可完成,新触发等待清理完成而不是被静默吞掉(ADR-16)。
    fn is_cancelling(&self) -> bool {
        self.cancelled
    }
}

/// 会话代际计数器:每次 `ActiveSession::new` 自增,用于丢弃旧壳/旧会话结果。
static SESSION_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_session_generation() -> u64 {
    SESSION_GENERATION.fetch_add(1, Ordering::SeqCst)
}

/// 看门狗:正常截取远小于 30s;busy 超时说明壳消息泵/线程卡死,
/// 强制重置旧会话,避免"取消一次后再也无法截取"的静默死锁。
const STALE_SESSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Cancelling 清理等待上限:触发落在取消清理窗口内时,等待这段时间让清理
/// 完成后开始新截取;超时给"正在取消"提示,不静默丢弃(ADR-16)。
const CANCEL_CLEARANCE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Cancelling 等待期间的轮询间隔(清理是本进程内的短操作)。
const CANCEL_CLEARANCE_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeginDecision {
    IgnoreBusy,
    /// 会话正在取消清理:短时等待后重试,而不是静默忽略。
    WaitForCancelClearance,
    ResetStaleThenBegin,
    Begin,
}

fn begin_decision(session: Option<&ActiveSession>, now: Instant) -> BeginDecision {
    match session {
        Some(current) if current.busy && current.is_cancelling() => {
            let cancelling_for = current
                .cancel_requested_at
                .map(|at| now.saturating_duration_since(at))
                .unwrap_or_default();
            if cancelling_for > STALE_SESSION_TIMEOUT {
                BeginDecision::ResetStaleThenBegin
            } else {
                BeginDecision::WaitForCancelClearance
            }
        }
        Some(current)
            if current.busy
                && now.saturating_duration_since(current.started_at) > STALE_SESSION_TIMEOUT =>
        {
            BeginDecision::ResetStaleThenBegin
        }
        Some(current) if current.busy => BeginDecision::IgnoreBusy,
        _ => BeginDecision::Begin,
    }
}

/// Occupies the session slot unless a live busy capture is still in flight.
/// Returns false when the overlapping request must be ignored.
#[cfg(test)]
fn occupy_session(
    slot: &mut Option<ActiveSession>,
    mode: CaptureMode,
    delay_ms: u64,
    now: Instant,
) -> bool {
    match begin_decision(slot.as_ref(), now) {
        BeginDecision::IgnoreBusy | BeginDecision::WaitForCancelClearance => false,
        BeginDecision::ResetStaleThenBegin | BeginDecision::Begin => {
            *slot = Some(ActiveSession::new(mode, delay_ms, now));
            true
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CancelOutcome {
    pub clipboard_written: bool,
    pub file_written: bool,
    pub preview_opened: bool,
}

impl CancelOutcome {
    pub fn clean() -> Self {
        Self {
            clipboard_written: false,
            file_written: false,
            preview_opened: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionSelection {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// How a finished capture is presented: `Preview` keeps today's behavior
/// (clipboard + preview window), `Quiet` stays silent and keeps the frame
/// in an idle session for a short TTL (ADR-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishDisposition {
    Preview,
    Quiet,
}

/// Actions a platform shell can request on a finished selection without
/// opening a workspace (R3). R2 起「取字」不再走静默动作:它提交区域并置
/// `pending_ocr` 打开工作区覆盖层,因此这里没有 Ocr。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuietAction {
    Copy,
    Save,
    Pin,
}

/// Retention window for the frame kept after a quiet finish.
pub const DEFAULT_FRAME_TTL: Duration = Duration::from_secs(30);

pub fn begin(app: &AppHandle, mode: CaptureMode, delay_ms: u64, target: FullscreenTarget) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run(app.clone(), mode, delay_ms, target).await {
            if !error.is_cancelled() {
                let _ = finish_error(&app, error);
            }
        }
    });
}

/// 托盘"上次区域"直取(R6):先按当前显示环境校验并钳制记录,再走与常规
/// 截取一致的隐藏前置与延时链路;无效记录给 toast 并让菜单回到禁用态,
/// 不打开交互选区。
pub fn begin_last_region(app: &AppHandle, delay_ms: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run_last_region(app.clone(), delay_ms).await {
            if !error.is_cancelled() {
                let _ = finish_error(&app, error);
            }
        }
    });
}

/// R23 阶段日志门控(与壳/平台侧同源):触发→hide→抓屏→壳→错误窗,
/// 足以区分"未进壳/挂起/阻塞/错误未显示"。
fn capture_timing_enabled() -> bool {
    std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some()
}

async fn run(
    app: AppHandle,
    mode: CaptureMode,
    delay_ms: u64,
    target: FullscreenTarget,
) -> Result<(), CaptureError> {
    let Some(generation) = try_begin_with_delay(&app, mode, delay_ms, target).await else {
        return Ok(());
    };
    if capture_timing_enabled() {
        eprintln!(
            "Cropmark capture: trigger mode={mode:?} delay_ms={delay_ms} generation={generation}"
        );
    }
    let result = run_capture(&app, mode, delay_ms, generation).await;
    if result.as_ref().is_err_and(|error| error.is_cancelled()) {
        cleanup_cancelled_generation(&app, generation);
        return Ok(());
    }
    result
}

async fn run_capture(
    app: &AppHandle,
    mode: CaptureMode,
    delay_ms: u64,
    generation: u64,
) -> Result<(), CaptureError> {
    // R1:长截图需要平台连续抓取能力;Wayland/portal 在隐藏产品界面之前
    // 明确失败并给出文案,不产生剪贴板/历史/磁盘输出。
    if mode == CaptureMode::LongCapture && !crate::settings::current_capture(app).long_capture {
        return Err(CaptureError::unavailable("error.capture.scroll_disabled"));
    }
    if mode == CaptureMode::LongCapture && !platform::scroll_capture_supported() {
        return Err(CaptureError::unavailable(
            "error.capture.scroll_unsupported",
        ));
    }
    let hide_started = Instant::now();
    hide_product_surfaces(app, generation)?;
    if capture_timing_enabled() {
        eprintln!(
            "Cropmark capture: hide done generation={generation} elapsed={:?}",
            hide_started.elapsed()
        );
    }
    wait_delay_before_capture(app, delay_ms, mode, generation).await?;
    match mode {
        CaptureMode::Region => capture_region(app, generation).await,
        CaptureMode::Window => capture_window_mode(app, generation).await,
        CaptureMode::Fullscreen => capture_fullscreen(app, generation).await,
        // R1:长截图复用区域选区壳选一个固定区域,确认后进入滚动会话。
        CaptureMode::LongCapture => capture_long_capture(app, generation).await,
        // R3:录屏复用区域选区壳;原生壳经「录屏」动作确认,Web 覆盖层
        // (Wayland)确认后由 `confirm_region` 分流进录制。
        CaptureMode::Recording => capture_region(app, generation).await,
    }
}

/// 隐藏前置完成后按延时计划展示并等待倒计时(可取消,代际不符即中止);
/// 0 秒立即返回。
async fn wait_delay_before_capture(
    app: &AppHandle,
    delay_ms: u64,
    mode: CaptureMode,
    generation: u64,
) -> Result<(), CaptureError> {
    // 录屏的倒计时发生在确认之后,矩形保持可见;截图仍按原计划先倒计时。
    if mode == CaptureMode::Recording {
        return Ok(());
    }
    let plan = plan_delay(delay_ms);
    if plan.delay_ms > 0 && !plan.overlay_during_delay {
        show_delay(app, plan.delay_ms, mode)?;
        if wait_delay(app, plan.delay_ms, generation).await? {
            hide_session_surface(app, ui::DELAY)?;
            hide_product_surfaces(app, generation)?;
        }
    }
    Ok(())
}

async fn run_last_region(app: AppHandle, delay_ms: u64) -> Result<(), CaptureError> {
    let plan = match plan_last_region(&app).await {
        Ok(plan) => plan,
        Err(reason) => {
            // 记录缺失或与当前显示环境无交集:清除记录并提示,菜单变为
            // "暂无记录"禁用态;不隐藏任何产品界面。
            crate::settings::forget_last_region(&app);
            ui::show_toast_key(&app, reason.key());
            return Ok(());
        }
    };
    let Some(generation) = try_begin_with_delay(
        &app,
        CaptureMode::Region,
        delay_ms,
        FullscreenTarget::Pointer,
    )
    .await
    else {
        return Ok(());
    };
    hide_product_surfaces(&app, generation)?;
    wait_delay_before_capture(&app, delay_ms, CaptureMode::Region, generation).await?;
    capture_last_region(&app, plan, generation).await
}

/// 上次区域使用前的校验失败:两种都意味着该项当前不可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastRegionPlanError {
    Missing,
    OutOfRange,
}

impl LastRegionPlanError {
    fn key(self) -> &'static str {
        match self {
            Self::Missing => "toast.last_region_missing",
            Self::OutOfRange => "toast.last_region_out_of_range",
        }
    }

    #[cfg(test)]
    fn toast(self) -> String {
        crate::i18n::t(self.key())
    }
}

/// 上次区域直取的执行计划:目标显示器 + 已钳制进该显示器的全局物理区域。
#[derive(Debug, Clone, PartialEq)]
struct FixedRegionPlan {
    monitor: MonitorGeom,
    region: crate::settings::LastRegion,
}

/// 读取记录并按当前显示器列表校验/钳制;记录不存在或与所有显示器都无
/// 交集时返回错误,调用方提示且不进入抓取。读取与显示器枚举都在阻塞线程。
async fn plan_last_region(app: &AppHandle) -> Result<FixedRegionPlan, LastRegionPlanError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let region =
            crate::settings::current_last_region(&handle).ok_or(LastRegionPlanError::Missing)?;
        let monitors = tauri_monitors(&handle);
        let rects: Vec<crate::settings::LastRegion> = monitors
            .iter()
            .map(|monitor| crate::settings::LastRegion {
                x: monitor.physical_x,
                y: monitor.physical_y,
                width: monitor.physical_width,
                height: monitor.physical_height,
            })
            .collect();
        let (index, clamped) = region
            .clamp_to_monitors(&rects)
            .ok_or(LastRegionPlanError::OutOfRange)?;
        Ok(FixedRegionPlan {
            monitor: monitors[index].clone(),
            region: clamped,
        })
    })
    .await
    .map_err(|_| LastRegionPlanError::OutOfRange)?
}

/// 抓取目标显示器并按钳制后的区域裁剪,把已定画面送进编辑页。
async fn capture_last_region(
    app: &AppHandle,
    plan: FixedRegionPlan,
    generation: u64,
) -> Result<(), CaptureError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_capture_ready(&handle)?;
        let cursor = cursor_mode(&handle);
        let (frame, outcome) = platform::capture_monitor_with_cursor(&plan.monitor, cursor)?;
        note_cursor(&handle, outcome);
        let (x, y, width, height) = local_crop(&plan.monitor, &plan.region)
            .ok_or_else(|| CaptureError::api("error.capture.last_region_out_of_range"))?;
        let cropped = crop_rgba(&frame, x, y, width, height)?;
        if !session_matches_generation(&handle, generation) {
            return Ok(());
        }
        deliver_fixed_frame(
            &handle,
            cropped,
            Vec::new(),
            Some(plan.monitor),
            Some(generation),
        )
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

/// 全局物理区域 → 显示器帧内局部裁剪坐标;计划已钳制,正常必成功,
/// 仍做边界检查保证任何异常数据都不越界。
fn local_crop(
    monitor: &MonitorGeom,
    region: &crate::settings::LastRegion,
) -> Option<(u32, u32, u32, u32)> {
    let x = i64::from(region.x) - i64::from(monitor.physical_x);
    let y = i64::from(region.y) - i64::from(monitor.physical_y);
    if x < 0 || y < 0 {
        return None;
    }
    let x = u32::try_from(x).ok()?;
    let y = u32::try_from(y).ok()?;
    if x.checked_add(region.width)? > monitor.physical_width
        || y.checked_add(region.height)? > monitor.physical_height
    {
        return None;
    }
    Some((x, y, region.width, region.height))
}

/// 触发入口的决策结果(在取消清理窗口内区分"等待重试"与"开始")。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeginStep {
    /// 已占用会话并分配代际,可以继续 hide/截取。
    Begun(u64),
    /// 另有活跃截取:本触发丢弃;若原生壳仍在,顺手发关闭让它自行取消。
    Ignored,
    /// 会话处于 Cancelling:等待清理完成后重试。
    WaitingForCancel,
}

/// 单次触发尝试:在同一把锁内决策并占用槽位。stale/取消看门狗重置时,
/// 旧会话被取出并在锁外收尾(关闭旧壳、恢复旧产品表面,ADR-16)。
fn place_session(
    slot: &mut Option<ActiveSession>,
    mode: CaptureMode,
    delay_ms: u64,
    now: Instant,
    target: FullscreenTarget,
) {
    let mut session = ActiveSession::new(mode, delay_ms, now);
    session.fullscreen_target = if mode == CaptureMode::Fullscreen {
        target
    } else {
        FullscreenTarget::Pointer
    };
    *slot = Some(session);
}

fn begin_capture(
    app: &AppHandle,
    mode: CaptureMode,
    delay_ms: u64,
    target: FullscreenTarget,
) -> BeginStep {
    let now = Instant::now();
    let mut stale: Option<ActiveSession> = None;
    let overlay_alive = overlay_still_open(app);
    let step = with_session_mut(app, |slot| match begin_decision(slot.as_ref(), now) {
        BeginDecision::IgnoreBusy | BeginDecision::WaitForCancelClearance if !overlay_alive => {
            // 实测:Esc 后原生壳窗口已销毁,但取消清理若卡在 Tauri 窗口 API,
            // 会话仍 busy/cancelling,下一次热键被丢掉。壳已不在就强制开新截取。
            stale = slot.take();
            place_session(slot, mode, delay_ms, now, target.clone());
            BeginStep::Begun(current_generation(slot))
        }
        BeginDecision::IgnoreBusy => BeginStep::Ignored,
        BeginDecision::WaitForCancelClearance => BeginStep::WaitingForCancel,
        BeginDecision::ResetStaleThenBegin => {
            stale = slot.take();
            place_session(slot, mode, delay_ms, now, target.clone());
            BeginStep::Begun(current_generation(slot))
        }
        BeginDecision::Begin => {
            place_session(slot, mode, delay_ms, now, target.clone());
            BeginStep::Begun(current_generation(slot))
        }
    });
    match step {
        BeginStep::Begun(generation) => {
            if let Some(stale) = stale {
                log::warn!("capture reset kind=stale id={generation}");
                eprintln!("Cropmark: stale busy session reset after {STALE_SESSION_TIMEOUT:?}");
                reset_stale_session(app, &stale);
            }
            log::info!(
                "capture start id={generation} mode={mode:?} delay_ms={delay_ms} target={}",
                fullscreen_target_kind(&target)
            );
            // A lingering toast must not leak into the next capture (hide-before-capture);
            // 仅隐藏——toast 窗是预创建复用的 webview,关闭会破坏复用。
            ui::hide_window(app, ui::TOAST);
            set_last_error(app, None);
            BeginStep::Begun(generation)
        }
        BeginStep::WaitingForCancel => {
            close_active_native_shell();
            BeginStep::WaitingForCancel
        }
        BeginStep::Ignored => {
            log::debug!("capture ignored mode={mode:?}");
            close_active_native_shell();
            BeginStep::Ignored
        }
    }
}

fn fullscreen_target_kind(target: &FullscreenTarget) -> &'static str {
    match target {
        FullscreenTarget::Pointer => "pointer",
        FullscreenTarget::Monitor(_) => "monitor",
        FullscreenTarget::All => "all",
    }
}

/// 槽位内当前会话的代际(调用方保证刚占用)。
fn current_generation(slot: &Option<ActiveSession>) -> u64 {
    slot.as_ref()
        .map(|current| current.generation)
        .unwrap_or_default()
}

/// 看门狗重置:关闭旧会话记录的原生壳、隐藏会话窗、恢复旧会话隐藏前的
/// 产品表面并广播取消,保证被替换的会话不残留窗口、旧壳结果不落到新会话。
fn reset_stale_session(app: &AppHandle, stale: &ActiveSession) {
    close_active_native_shell();
    for label in ui::session_window_labels() {
        ui::hide_window(app, label);
    }
    for surface in stale.hide.restore_on_cancel() {
        ui::show_window(app, &surface.label);
    }
    let _ = app.emit("capture-cancelled", ());
    crate::pin::restore_after_capture(app);
}

/// 关闭当前原生选区壳(若有):stale 重置时让旧壳退出,其随后返回的结果
/// 因代际不符被丢弃。非原生平台为空实现。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn close_active_native_shell() {
    super::native_overlay::request_shell_close();
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn close_active_native_shell() {}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn native_shell_active() -> bool {
    super::native_overlay::shell_is_active()
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn native_shell_active() -> bool {
    false
}

fn overlay_still_open(app: &AppHandle) -> bool {
    if native_shell_active() {
        return true;
    }
    // Windows/macOS 区域截取走原生壳;预创建的 Web overlay 若仍报可见,
    // 会把「壳已关、会话还忙」误判成还在截取,第二次热键就被丢掉。
    if region_native_shell() {
        return false;
    }
    ui::is_visible(app, ui::OVERLAY)
}

/// 占用会话槽位或短时等待取消清理完成;超时 toast 提示"正在取消"(ADR-16)。
async fn try_begin_with_delay(
    app: &AppHandle,
    mode: CaptureMode,
    delay_ms: u64,
    target: FullscreenTarget,
) -> Option<u64> {
    let deadline = Instant::now() + CANCEL_CLEARANCE_TIMEOUT;
    loop {
        match begin_capture(app, mode, delay_ms, target.clone()) {
            BeginStep::Begun(generation) => return Some(generation),
            BeginStep::Ignored => return None,
            BeginStep::WaitingForCancel => {
                if Instant::now() >= deadline {
                    ui::show_toast_key(app, "toast.cancel_in_progress");
                    return None;
                }
                let _ = tauri::async_runtime::spawn_blocking(|| {
                    std::thread::sleep(CANCEL_CLEARANCE_POLL);
                })
                .await;
            }
        }
    }
}

fn hide_product_surfaces(app: &AppHandle, generation: u64) -> Result<(), CaptureError> {
    let mut recorded = Vec::new();
    for label in ui::product_window_labels() {
        let visible = ui::is_visible(app, label);
        if visible {
            ui::hide_window(app, label);
        }
        if let Some(kind) = SurfaceKind::from_label(label) {
            recorded.push(RecordedSurface {
                label: label.to_string(),
                kind,
                was_visible: visible,
            });
        }
    }
    let tray_open = platform::tray_popup_visible();
    if tray_open {
        recorded.push(RecordedSurface {
            label: "tray-popup".into(),
            kind: SurfaceKind::TrayPopup,
            was_visible: true,
        });
    }
    platform::dismiss_tray_popup();
    crate::pin::lower_for_capture(app);
    for label in ui::session_window_labels() {
        if label != ui::DELAY && ui::is_visible(app, label) {
            ui::hide_window(app, label);
        }
    }
    // 先落盘已隐藏记录再等待:即使随后等待超时返回错误,取消/错误路径也能
    // 按记录恢复旧产品表面(ADR-16)。代际不符(旧会话已被替换)立即中止,
    // 旧运行不得写入新会话的 hide 记录。
    with_session_mut(app, |session| {
        let Some(session) = session.as_mut() else {
            return Err(CaptureError::cancelled());
        };
        if session.generation != generation || session.cancelled {
            return Err(CaptureError::cancelled());
        }
        if session.hide.recorded.is_empty() {
            session.hide = HideWait::record(recorded.clone());
        }
        session.hide.request_hide();
        Ok(())
    })?;
    let visible_labels: Vec<&str> = recorded
        .iter()
        .filter(|surface| surface.was_visible && surface.label != "tray-popup")
        .map(|surface| surface.label.as_str())
        .collect();
    if !visible_labels.is_empty() {
        let hidden = wait_until_hidden(
            || ui::any_visible(app, &visible_labels),
            Duration::from_millis(160),
        );
        if !hidden {
            return Err(hide_not_presented_error());
        }
    }
    wait_compositor_presented();
    with_session_mut(app, |session| {
        let Some(session) = session.as_mut() else {
            return Err(CaptureError::cancelled());
        };
        if session.generation != generation || session.cancelled {
            return Err(CaptureError::cancelled());
        }
        session.hide.commit_presented(true, true)
    })
}

fn hide_session_surface(app: &AppHandle, label: &str) -> Result<(), CaptureError> {
    ui::hide_window(app, label);
    let hidden = wait_until_hidden(|| ui::is_visible(app, label), Duration::from_millis(400));
    if !hidden {
        return Err(hide_not_presented_error());
    }
    wait_compositor_presented();
    Ok(())
}

fn show_delay(app: &AppHandle, delay_ms: u64, mode: CaptureMode) -> Result<(), CaptureError> {
    ui::open_delay(app, delay_ms)?;
    let _ = app.emit("capture-delay", DelayPayload { delay_ms, mode });
    Ok(())
}

async fn wait_delay(app: &AppHandle, delay_ms: u64, generation: u64) -> Result<bool, CaptureError> {
    let steps = (delay_ms / 100).max(1);
    for _ in 0..steps {
        if is_cancelled(app) {
            cancel_internal(app, None)?;
            return Err(CaptureError::cancelled());
        }
        // 旧会话被看门狗替换后立即中止倒计时,不再触碰新会话(ADR-16)。
        if !session_matches_generation(app, generation) {
            return Err(CaptureError::cancelled());
        }
        let _ = tauri::async_runtime::spawn_blocking(|| {
            std::thread::sleep(Duration::from_millis(100));
        })
        .await;
    }
    Ok(true)
}

// 选区壳回调线程:壳与窗口消息泵在同一阻塞线程内同步运行,回调经此
// thread-local 取回 AppHandle(壳保持平台/运行时无关)。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
thread_local! {
    static SHELL_APP: std::cell::RefCell<Option<AppHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// C 键取色回调:复制 HEX+RGB 文本并 toast 反馈;剪贴板失败提示失败。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn copy_color_feedback(text: &str, hex: &str) {
    if capture_timing_enabled() {
        eprintln!("Cropmark color copy: hook ran, hex={hex}");
    }
    let toast_key = |key: &str, params: &[(&str, &str)]| {
        SHELL_APP.with(|slot| {
            if let Some(app) = slot.borrow().as_ref() {
                ui::show_toast_key_params(app, key, params);
            } else if capture_timing_enabled() {
                eprintln!("Cropmark color copy: no app in thread-local");
            }
        });
    };
    match clipboard::copy_text(text) {
        Ok(()) => toast_key("toast.color_copied", &[("hex", hex)]),
        Err(_) => toast_key("toast.color_copy_failed", &[]),
    }
}

/// R21:选区即时标注的样式与文本输入能力。样式沿用 `AnnotationDefaults`
/// (R8 记忆);三平台原生壳都具备文本输入通道(Windows WM_CHAR/IME、
/// macOS NSTextInputClient、Linux X11 XIM+直输回退),工具条含文字工具。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn annotation_options_from(app: &AppHandle) -> super::selection::AnnotationOptions {
    let defaults = crate::settings::current_annotation_defaults(app);
    super::selection::AnnotationOptions {
        color: defaults.color,
        stroke_width: defaults.width,
        text_size: defaults.text_size,
        number_start: defaults.number_start,
        text_input: true,
    }
}

/// 原生壳区域路径(Windows/macOS/Linux X11):冻结指针所在屏像素并交给
/// 平台壳,按壳结果走 Preview/Quiet/取消分发(三平台同构,ADR-008)。
/// 结果分发前校验会话代际:被 stale 重置替换后,旧壳结果直接丢弃(ADR-16)。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
async fn capture_region_native(app: &AppHandle, generation: u64) -> Result<(), CaptureError> {
    use super::native_overlay::RegionOutcome;

    let handle = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let (frame, monitor) = grab_pointer_screen(&handle)?;
        store_pixels(&handle, frame.clone(), monitor.clone(), generation)?;
        // R19:旧入口开关(取字/贴图/复制/保存/放大镜/光标提示/即时标注)已按
        // 常开语义移除;R9:长截图与全部标注工具去门控常开,普通区域选区固定
        // 传入全开的能力集;R3:录屏动作由设置开关门控,录屏模式只保留「录屏」
        // 确认路径(与长截图同构,避免选区内标注的屏幕坐标与录制区域错位)。
        let mode = with_session(&handle, |session| {
            session.as_ref().map(|current| current.mode)
        })
        .unwrap_or(CaptureMode::Region);
        let mut flags = recording_selection_flags(
            mode,
            crate::settings::current_recording(&handle).enabled,
            crate::settings::current_capture(&handle).long_capture,
        );
        let region_tools = crate::settings::current_region_tools(&handle);
        flags.tools = region_tools_from_settings(region_tools);
        flags.ocr_entry = region_tools.ocr;
        flags.qr_entry = region_tools.qr;
        // 贴图固定出现(录屏/长截图壳自己关掉 toolbar_pin)。直线跟随箭头,模糊跟随马赛克。
        if flags.toolbar_pin {
            flags.pin_entry = true;
        }
        flags.mode_line = flags.tools.arrow;
        flags.mode_blur = flags.tools.mosaic;
        if mode == CaptureMode::Recording {
            flags.record_even = crate::settings::current_recording(&handle).format
                == crate::record::RecordFormat::Mp4;
            flags.confirm_delay_ms = crate::settings::current_capture(&handle).delay_ms();
        }
        // R7:元素检测完全不可用时按现有能力说明机制提示降级(自由框选或
        // 窗口截取模式);LongCapture/Recording 复用本壳但不需要该说明。
        if mode == CaptureMode::Region {
            if let Some(key) = snap_fallback_notice_key(super::snap::platform_capability()) {
                ui::show_toast_key(&handle, key);
            }
        }
        let annotation_options = annotation_options_from(&handle);
        // 壳回调在同一线程内同步执行,经 thread-local 取回 AppHandle。
        SHELL_APP.with(|slot| *slot.borrow_mut() = Some(handle.clone()));
        let picked = super::native_overlay::pick_region(
            &frame,
            &monitor,
            flags,
            annotation_options,
            super::native_overlay::ShellHooks {
                copy_color: copy_color_feedback,
            },
        );
        SHELL_APP.with(|slot| *slot.borrow_mut() = None);
        picked
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))??;
    if capture_timing_enabled() {
        eprintln!("Cropmark capture: region shell outcome={picked:?} generation={generation}");
    }
    if !session_matches_generation(app, generation) {
        if capture_timing_enabled() {
            eprintln!("Cropmark capture: region shell result discarded (stale generation)");
        }
        cleanup_cancelled_generation(app, generation);
        return Ok(());
    }
    // 以长截图模式进入时,Enter/「标注」确认都转为开始滚动会话。
    let long_mode = with_session(app, |session| {
        session
            .as_ref()
            .is_some_and(|current| current.mode == CaptureMode::LongCapture)
    });
    // R3:以录屏模式进入时,Enter/「标注」确认都转为开始录制会话。
    let recording_mode = with_session(app, |session| {
        session
            .as_ref()
            .is_some_and(|current| current.mode == CaptureMode::Recording)
    });
    match picked {
        RegionOutcome::Recording(rect, annotations) => {
            start_recording_from_shell(app, rect, annotations, generation)
        }
        RegionOutcome::Preview(rect, annotations) | RegionOutcome::Annotate(rect, annotations)
            if recording_mode =>
        {
            start_recording_from_shell(app, rect, annotations, generation)
        }
        RegionOutcome::LongCapture(rect, annotations, axis) => {
            start_scroll_session(app, rect, annotations, axis, generation)
        }
        RegionOutcome::Preview(rect, annotations) | RegionOutcome::Annotate(rect, annotations)
            if long_mode =>
        {
            start_scroll_session(
                app,
                rect,
                annotations,
                super::scroll::CaptureAxis::default(),
                generation,
            )
        }
        RegionOutcome::Preview(rect, annotations) | RegionOutcome::Annotate(rect, annotations) => {
            commit_region_to_workspace(app, rect, annotations, false, false, generation).await
        }
        // R2:壳上的「取字」不再静默复制;提交区域并置 `pending_ocr`,工作区
        // 覆盖层打开后自动进入取字。取字前不写剪贴板(含不再先写 PNG)。
        RegionOutcome::Ocr(rect, annotations) => {
            commit_region_to_workspace(app, rect, annotations, true, false, generation).await
        }
        // R4:壳上的「识别二维码」提交区域并置 `pending_qr`。二维码打开按图片
        // 大小的预览并自动识别,不进入全屏冻结层;不写剪贴板、不打开链接。
        RegionOutcome::Qr(rect, annotations) => {
            commit_region_to_workspace(app, rect, annotations, false, true, generation).await
        }
        // 复制/保存/贴图在选区上直接完成,不打开工作区。
        RegionOutcome::Quiet(rect, action, annotations) => {
            finish_region_with(
                app,
                RegionSelection {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
                annotations,
                action,
                Some(generation),
            )
            .await
        }
        RegionOutcome::Cancelled => {
            // 壳已经退出,不要再 PostMessage(WM_CLOSE):否则会关掉紧接着
            // 第二次热键建起来的新选区窗(实测 Esc 后再截覆盖层出不来)。
            if let Some((generation, restore)) = accept_cancel(app, Some(generation)) {
                finish_cancel(app, generation, restore, false);
            }
            Ok(())
        }
    }
}

/// R2/R4:原生壳把已确认区域交给工作区覆盖层。`pending_ocr` / `pending_qr`
/// 为 true 时覆盖层打开后自动进入取字/二维码识别;不写剪贴板、不打开预览。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
async fn commit_region_to_workspace(
    app: &AppHandle,
    rect: super::geometry::PhysicalRect,
    annotations: Vec<Annotation>,
    pending_ocr: bool,
    pending_qr: bool,
    generation: u64,
) -> Result<(), CaptureError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        commit_shell_region(
            &handle,
            RegionSelection {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            },
            annotations,
            pending_ocr,
            pending_qr,
            generation,
        )
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))??;
    Ok(())
}

/// R1:选区确认后交给滚动会话(固定区域周期抓取 + 按轴拼接 + 控制窗)。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn start_scroll_session(
    app: &AppHandle,
    rect: super::geometry::PhysicalRect,
    annotations: Vec<Annotation>,
    axis: super::scroll::CaptureAxis,
    generation: u64,
) -> Result<(), CaptureError> {
    super::scroll::start(
        app,
        generation,
        RegionSelection {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        },
        annotations,
        axis,
    )
}

/// R1 长截图模式:与区域截取共用原生选区壳;壳确认后进入滚动会话。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
async fn capture_long_capture(app: &AppHandle, generation: u64) -> Result<(), CaptureError> {
    capture_region_native(app, generation).await
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
async fn capture_long_capture(_app: &AppHandle, _generation: u64) -> Result<(), CaptureError> {
    Err(CaptureError::unavailable(
        "error.capture.scroll_unsupported",
    ))
}

/// R3:原生选区壳确认(「录屏」动作或 Enter/「标注」)→ 会话层启动录制。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn start_recording_from_shell(
    app: &AppHandle,
    rect: super::geometry::PhysicalRect,
    annotations: Vec<Annotation>,
    generation: u64,
) -> Result<(), CaptureError> {
    start_recording_from_selection(
        app,
        RegionSelection {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        },
        annotations,
        Some(generation),
    )
}

/// R3:录屏入口的选区确认 → 启动录制会话。开关关闭、已有录制或启动失败时
/// 不产生录制行为,并按取消收尾(恢复产品表面并释放选区槽位)。
fn start_recording_from_selection(
    app: &AppHandle,
    selection: RegionSelection,
    annotations: Vec<Annotation>,
    expected: Option<u64>,
) -> Result<(), CaptureError> {
    let recording_settings = crate::settings::current_recording(app);
    if !recording_settings.enabled {
        return cancel_recording_entry(app, expected);
    }
    if recording_active(app) {
        ui::show_toast_key(app, "toast.recording_busy");
        return cancel_recording_entry(app, expected);
    }
    let monitor = with_session(app, |session| {
        session.as_ref().and_then(|current| current.monitor.clone())
    });
    let Some(monitor) = monitor else {
        return cancel_recording_entry(app, expected);
    };
    let region = crate::record::RecordRegion::new(
        selection.x,
        selection.y,
        selection.width,
        selection.height,
    );
    let config = crate::record::RecordConfig::from_settings(app, recording_settings.format);
    // 壳上已确认的标注跟着选区平移到录制区域坐标系,首次抓帧即合并进画面;
    // 录制中的实时标注由后续 HUD 经 `with_recording` 同步。
    let translated =
        crate::annotate::translated_all(&annotations, -(selection.x as f64), -(selection.y as f64));
    let recording = match crate::record::RecordingSession::start(
        region,
        config,
        crate::record::MonitorSource::new(monitor.clone()),
    ) {
        Ok(recording) => recording,
        Err(error) => {
            ui::show_toast(app, &error.user_message());
            return cancel_recording_entry(app, expected);
        }
    };
    recording.set_annotations(translated);
    {
        let runtime = app.state::<CaptureRuntime>();
        *lock(&runtime.recording) = Some(recording);
    }
    // 选区会话正常结束(不是取消):隐藏会话窗、恢复产品表面并释放槽位,
    // 录制在后台独立继续;不广播 capture-cancelled。
    release_capture_for_recording(app, expected);
    // R3:录制开始即打开控制条与标注层(位置/内容由 record/hud 决定)。
    crate::record::hud::open(app, region, monitor);
    ui::show_toast_key(app, "toast.recording_started");
    refresh_tray_menu(app);
    Ok(())
}

/// 不进入录制时的收尾(开关关闭/已有录制/启动失败):按取消语义释放选区
/// 会话并恢复产品表面。
fn cancel_recording_entry(app: &AppHandle, expected: Option<u64>) -> Result<(), CaptureError> {
    if let Some((generation, restore)) = accept_cancel(app, expected) {
        finish_cancel(app, generation, restore, false);
    }
    Ok(())
}

/// 录制已启动:选区会话正常结束,隐藏会话窗并恢复此前隐藏的产品表面,
/// 释放槽位等待后续截图。代际不符(旧壳)时不触碰新会话。
fn release_capture_for_recording(app: &AppHandle, expected: Option<u64>) {
    let restore = with_session(app, |session| {
        session
            .as_ref()
            .filter(|current| !generation_mismatch(current, expected))
            .map(|current| current.hide.restore_on_cancel())
            .unwrap_or_default()
    });
    for label in ui::session_window_labels() {
        ui::hide_window(app, label);
    }
    for surface in restore {
        ui::show_window(app, &surface.label);
    }
    crate::pin::restore_after_capture(app);
    with_session_mut(app, |session| {
        if session
            .as_ref()
            .is_some_and(|current| !generation_mismatch(current, expected))
        {
            *session = None;
        }
    });
}

/// R3:是否存在活动录制会话(托盘在「录屏」与「停止录屏并保存」之间切换)。
pub fn recording_active(app: &AppHandle) -> bool {
    with_recording(app, |_| ()).is_some()
}

/// R3:取出活动录制会话(停止/保存路径消费);同一时间最多一个。
/// 会话一离开运行时即复位 HUD 绘制交互,避免标注层继续挡鼠标。
pub fn take_recording_session(app: &AppHandle) -> Option<crate::record::RecordingSession> {
    let runtime = app.state::<CaptureRuntime>();
    let taken = lock(&runtime.recording).take();
    if taken.is_some() {
        crate::record::hud::reset_after_session_end(app);
    }
    taken
}

/// R3 recording-hud:把 HUD 在无活动会话时新启动的录制装入运行时。
/// 已有会话时不覆盖,返回 false(新会话由调用方丢弃,其 Drop 会停止线程)。
pub fn install_recording(app: &AppHandle, recording: crate::record::RecordingSession) -> bool {
    let runtime = app.state::<CaptureRuntime>();
    let mut slot = lock(&runtime.recording);
    if slot.is_some() {
        return false;
    }
    *slot = Some(recording);
    true
}

/// R3:录制会话的 HUD 面(后续 recording-hud 消费):状态查询、暂停/继续与
/// 实时标注同步都经这里拿到会话引用;入口任务先定义,避免跨 scope 修改本文件。
pub fn with_recording<R>(
    app: &AppHandle,
    f: impl FnOnce(&crate::record::RecordingSession) -> R,
) -> Option<R> {
    let runtime = app.state::<CaptureRuntime>();
    let guard = lock(&runtime.recording);
    guard.as_ref().map(f)
}

/// 录制开始/结束后托盘入口需要切换(菜单 API 需在主线程执行)。
pub(crate) fn refresh_tray_menu(app: &AppHandle) {
    let handle = app.clone();
    let task = app.clone();
    let _ = handle.run_on_main_thread(move || crate::tray::refresh_menu(&task));
}

#[cfg(any(windows, target_os = "macos"))]
async fn capture_region(app: &AppHandle, generation: u64) -> Result<(), CaptureError> {
    capture_region_native(app, generation).await
}

/// Linux 区域路径按会话类型分派:判定复用 `platform::linux_capture_backend`
/// (WAYLAND_DISPLAY 非空 → portal/Wayland),并额外要求 `$DISPLAY` 可用
/// (含 XWayland);其余情况与既有行为一致走 Web 覆盖层(portal 抓屏不变)。
#[cfg(target_os = "linux")]
async fn capture_region(app: &AppHandle, generation: u64) -> Result<(), CaptureError> {
    if linux_uses_native_selection() {
        return capture_region_native(app, generation).await;
    }
    let monitor = freeze_screen(app, Vec::new(), generation).await?;
    ui::open_overlay(app, &monitor)?;
    Ok(())
}

/// 与 `platform/linux.rs` 的 backend 判定保持同源:Wayland 会话一律走
/// Web 覆盖层,非 Wayland 且 `$DISPLAY` 可用才启用 X11 原生壳。
#[cfg(target_os = "linux")]
fn linux_uses_native_selection() -> bool {
    let wayland = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|value| !value.is_empty());
    let display = std::env::var("DISPLAY").unwrap_or_default();
    !display.is_empty()
        && platform::linux_capture_backend(wayland.as_deref()) == platform::LinuxCaptureBackend::X11
}

/// 区域截取是否有原生选区壳(操作条/放大镜/取色/微调):Windows/macOS 恒有,
/// Linux 仅 X11 会话有;其余平台只有 Web 覆盖层。
fn region_native_shell() -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        linux_uses_native_selection()
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// R13:Web 覆盖层区域路径是否缺少原生能力,需要向用户说明不可用能力与
/// 替代方式。区域模式且无原生壳(即 Wayland/Linux 网页路径)时为 true;
/// 窗口模式的 Web 覆盖层不提供原生壳能力也不展示该说明,避免误导。
fn overlay_reduced_capabilities(mode: CaptureMode, native_shell: bool) -> bool {
    mode == CaptureMode::Region && !native_shell
}

/// R7:元素检测完全不可用时的降级说明词条(自由框选/窗口模式替代路径);
/// 窗口级仍可用(WindowOnly,如 macOS 未授权)不在此重复说明,授权与降级
/// 提示由平台任务在自己的流程里给出。
fn snap_fallback_notice_key(capability: super::snap::SnapCapability) -> Option<&'static str> {
    if capability.window_level() {
        None
    } else {
        capability.reason_key()
    }
}

/// R3/R9:选区壳能力集。
/// - 长截图模式:只保留「开始长截图」确认路径(壳动作同现状);
/// - 录屏模式:只保留「录屏」确认路径,避免选区内标注的屏幕坐标与录制区域
///   错位(与长截图同构);
/// - 普通区域模式:长截图和录屏都由设置开关门控,默认关闭。
fn region_tools_from_settings(tools: crate::settings::RegionTools) -> super::selection::ToolToggles {
    super::selection::ToolToggles {
        arrow: tools.arrow,
        rect: tools.rect,
        ellipse: tools.ellipse,
        highlighter: tools.highlighter,
        mosaic: tools.mosaic,
        text: tools.text,
        number: tools.number,
        spotlight: tools.spotlight,
        magnifier: tools.magnifier,
        bubble: tools.bubble,
        sticker: tools.sticker,
        erase: tools.erase,
    }
}

fn recording_selection_flags(
    mode: CaptureMode,
    recording_enabled: bool,
    long_capture_enabled: bool,
) -> super::selection::FeatureFlags {
    match mode {
        CaptureMode::LongCapture => super::selection::FeatureFlags {
            long_capture: true,
            inline_annotation: false,
            ocr_entry: false,
            pin_entry: false,
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
            ..super::selection::FeatureFlags::default()
        },
        CaptureMode::Recording => super::selection::FeatureFlags {
            recording: true,
            inline_annotation: false,
            ocr_entry: false,
            pin_entry: false,
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
            ..super::selection::FeatureFlags::default()
        },
        _ => super::selection::FeatureFlags {
            long_capture: long_capture_enabled,
            recording: recording_enabled,
            ..super::selection::FeatureFlags::default()
        },
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
async fn capture_region(app: &AppHandle, generation: u64) -> Result<(), CaptureError> {
    let monitor = freeze_screen(app, Vec::new(), generation).await?;
    ui::open_overlay(app, &monitor)?;
    Ok(())
}

/// 空/不可用窗口列表的错误词条键(R13:提示必须给出替代路径)。
const EMPTY_WINDOW_LIST_KEY: &str = "error.capture.empty_window_list";

/// Wayland/portal 列窗失败与空列表都走错误说明,不打开空白预览。
fn windows_for_window_mode(
    listed: Result<Vec<ListedWindow>, CaptureError>,
) -> Result<Vec<ListedWindow>, CaptureError> {
    let windows = listed?;
    if windows.is_empty() {
        return Err(CaptureError::unavailable(EMPTY_WINDOW_LIST_KEY));
    }
    Ok(windows)
}

async fn capture_window_mode(app: &AppHandle, generation: u64) -> Result<(), CaptureError> {
    let windows =
        tauri::async_runtime::spawn_blocking(move || platform::list_windows(platform::self_pid()))
            .await
            .map_err(|_| CaptureError::api("error.capture.window_list"))?;
    let windows = windows_for_window_mode(windows)?;
    let monitor = freeze_screen(app, windows, generation).await?;
    ui::open_overlay(app, &monitor)?;
    Ok(())
}

async fn capture_fullscreen(app: &AppHandle, generation: u64) -> Result<(), CaptureError> {
    let requested = with_session(app, |session| {
        session
            .as_ref()
            .map(|current| current.fullscreen_target.clone())
            .unwrap_or(FullscreenTarget::Pointer)
    });
    let target = effective_fullscreen_target(
        crate::settings::current_capture(app).multi_monitor,
        requested,
    );
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // R2:全屏结果同样停在冻结帧工作区覆盖层;带上落点显示器几何供浮层定位。
        let (frame, monitor) = match target {
            FullscreenTarget::Pointer => grab_pointer_screen(&handle)?,
            FullscreenTarget::Monitor(key) => capture_keyed_monitor(&handle, &key)?,
            FullscreenTarget::All => capture_all_monitors(&handle)?,
        };
        if !session_matches_generation(&handle, generation) {
            return Ok(());
        }
        deliver_fixed_frame(&handle, frame, Vec::new(), Some(monitor), Some(generation))
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

async fn freeze_screen(
    app: &AppHandle,
    windows: Vec<ListedWindow>,
    generation: u64,
) -> Result<MonitorGeom, CaptureError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (frame, monitor) = grab_pointer_screen(&handle)?;
        store_freeze(&handle, frame, monitor.clone(), windows, generation)?;
        Ok(monitor)
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

fn grab_pointer_screen(app: &AppHandle) -> Result<(Frame, MonitorGeom), CaptureError> {
    require_capture_ready(app)?;
    announce_screen_permission_wait(app);
    if capture_timing_enabled() {
        eprintln!("Cropmark capture: grab start");
    }
    let started = Instant::now();
    let monitor = tauri_pointer_monitor(app).unwrap_or(platform::pointer_monitor()?);
    let cursor = cursor_mode(app);
    let (frame, outcome) = platform::capture_monitor_with_cursor(&monitor, cursor)?;
    note_cursor(app, outcome);
    if capture_timing_enabled() {
        eprintln!(
            "Cropmark capture: grab done {}x{} scale={} elapsed={:?}",
            frame.width,
            frame.height,
            monitor.scale,
            started.elapsed()
        );
    }
    Ok((frame, monitor))
}

fn cursor_mode(app: &AppHandle) -> platform::CursorMode {
    if !crate::settings::current_capture(app).capture_cursor {
        return platform::CursorMode::Off;
    }
    let long = with_session(app, |session| {
        session
            .as_ref()
            .is_some_and(|current| current.mode == CaptureMode::LongCapture)
    });
    if long {
        platform::CursorMode::Off
    } else {
        platform::CursorMode::WhenInside
    }
}

fn note_cursor(app: &AppHandle, outcome: platform::CursorOutcome) {
    let unavailable = outcome == platform::CursorOutcome::Unavailable;
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            current.cursor_unavailable = unavailable;
        }
    });
    if unavailable {
        eprintln!("Cropmark capture: cursor unavailable");
    }
}

fn take_cursor_unavailable(app: &AppHandle) -> bool {
    with_session_mut(app, |session| {
        session.as_mut().is_some_and(|current| {
            let unavailable = current.cursor_unavailable;
            current.cursor_unavailable = false;
            unavailable
        })
    })
}

fn capture_keyed_monitor(app: &AppHandle, key: &str) -> Result<(Frame, MonitorGeom), CaptureError> {
    let monitors = tauri_monitors(app);
    let monitor = monitors
        .iter()
        .find(|monitor| monitor_key(monitor) == key)
        .cloned()
        .ok_or_else(|| CaptureError::unavailable("error.capture.monitor_unavailable"))?;
    let (frame, outcome) = platform::capture_display(&monitor, &monitors, cursor_mode(app))?;
    note_cursor(app, outcome);
    Ok((frame, monitor))
}

fn capture_all_monitors(app: &AppHandle) -> Result<(Frame, MonitorGeom), CaptureError> {
    let monitors = tauri_monitors(app);
    if monitors.is_empty() {
        return Err(CaptureError::unavailable(
            "error.capture.monitor_unavailable",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let wayland = std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|value| !value.is_empty());
        if platform::linux_capture_backend(wayland.as_deref())
            == platform::LinuxCaptureBackend::Portal
        {
            return capture_all_from_portal(app, &monitors);
        }
    }
    capture_all_per_display(app, &monitors)
}

/// 拼接虚拟画布对应的落点几何:工作区覆盖层按它定位与定尺寸。
fn virtual_monitor(canvas: &VirtualCanvas) -> MonitorGeom {
    MonitorGeom::from_physical(
        "virtual",
        canvas.origin_x,
        canvas.origin_y,
        canvas.width,
        canvas.height,
        1.0,
    )
}

fn capture_all_per_display(
    app: &AppHandle,
    monitors: &[MonitorGeom],
) -> Result<(Frame, MonitorGeom), CaptureError> {
    let canvas = virtual_canvas(monitors).map_err(canvas_error)?;
    let mode = cursor_mode(app);
    let mut frames = Vec::with_capacity(monitors.len());
    let mut outcome = platform::CursorOutcome::NotRequested;
    for monitor in monitors {
        let (frame, next) = platform::capture_display(monitor, monitors, mode)?;
        if frame.width != monitor.physical_width || frame.height != monitor.physical_height {
            return Err(CaptureError::unavailable("error.capture.stitch_failed"));
        }
        outcome = platform::merge_cursor_outcome(outcome, next);
        frames.push((monitor_dest(&canvas, monitor), frame));
    }
    let layers: Vec<(i32, i32, RgbaView<'_>)> = frames
        .iter()
        .map(|((x, y), frame)| {
            (
                *x,
                *y,
                RgbaView {
                    width: frame.width,
                    height: frame.height,
                    rgba: frame.rgba.as_slice(),
                },
            )
        })
        .collect();
    let rgba = stitch_views(&canvas, &layers, STITCH_BACKGROUND)
        .ok_or_else(|| CaptureError::unavailable("error.capture.stitch_failed"))?;
    note_cursor(app, outcome);
    Ok((
        Frame {
            width: canvas.width,
            height: canvas.height,
            rgba,
            scale: 1.0,
        },
        virtual_monitor(&canvas),
    ))
}

#[cfg(target_os = "linux")]
fn capture_all_from_portal(
    app: &AppHandle,
    monitors: &[MonitorGeom],
) -> Result<(Frame, MonitorGeom), CaptureError> {
    let canvas = virtual_canvas(monitors).map_err(canvas_error)?;
    let desktop = platform::capture_portal_desktop()?;
    let outcome = match cursor_mode(app) {
        platform::CursorMode::Off => platform::CursorOutcome::NotRequested,
        platform::CursorMode::WhenInside => platform::CursorOutcome::Unavailable,
    };
    if desktop.width == canvas.width && desktop.height == canvas.height {
        note_cursor(app, outcome);
        return Ok((
            Frame {
                width: desktop.width,
                height: desktop.height,
                rgba: desktop.rgba,
                scale: 1.0,
            },
            virtual_monitor(&canvas),
        ));
    }
    let mut frames = Vec::with_capacity(monitors.len());
    for monitor in monitors {
        let x = i64::from(monitor.physical_x) - i64::from(canvas.origin_x);
        let y = i64::from(monitor.physical_y) - i64::from(canvas.origin_y);
        if x < 0 || y < 0 {
            return Err(CaptureError::unavailable(
                "error.capture.stitch_unsupported",
            ));
        }
        let cropped = crop_rgba(
            &desktop,
            x as u32,
            y as u32,
            monitor.physical_width,
            monitor.physical_height,
        )
        .map_err(|_| CaptureError::unavailable("error.capture.stitch_unsupported"))?;
        frames.push((monitor_dest(&canvas, monitor), cropped));
    }
    let layers: Vec<(i32, i32, RgbaView<'_>)> = frames
        .iter()
        .map(|((x, y), frame)| {
            (
                *x,
                *y,
                RgbaView {
                    width: frame.width,
                    height: frame.height,
                    rgba: frame.rgba.as_slice(),
                },
            )
        })
        .collect();
    let rgba = stitch_views(&canvas, &layers, STITCH_BACKGROUND)
        .ok_or_else(|| CaptureError::unavailable("error.capture.stitch_unsupported"))?;
    note_cursor(app, outcome);
    Ok((
        Frame {
            width: canvas.width,
            height: canvas.height,
            rgba,
            scale: 1.0,
        },
        virtual_monitor(&canvas),
    ))
}

fn canvas_error(fault: CanvasFault) -> CaptureError {
    match fault {
        CanvasFault::Empty => CaptureError::unavailable("error.capture.monitor_unavailable"),
        CanvasFault::TooLarge => CaptureError::unavailable("error.capture.desktop_too_large"),
    }
}

/// R23:macOS 首次触发且尚未授权时,在系统授权弹窗出现前后给出过渡反馈,
/// 避免"毫无反应";已授权或已请求过不打扰(错误窗/正常流程自会反馈)。
#[cfg(target_os = "macos")]
fn announce_screen_permission_wait(app: &AppHandle) {
    use super::platform::{screen_permission_state, ScreenPermissionState};
    if screen_permission_state() == ScreenPermissionState::NotRequested {
        if capture_timing_enabled() {
            eprintln!("Cropmark capture: permission not requested; showing waiting toast");
        }
        ui::show_toast_key(app, "toast.screen_permission_waiting");
    }
}

#[cfg(not(target_os = "macos"))]
fn announce_screen_permission_wait(_app: &AppHandle) {}

fn require_capture_ready(app: &AppHandle) -> Result<(), CaptureError> {
    with_session(app, |session| {
        let wait = session
            .as_ref()
            .map(|current| &current.hide)
            .ok_or_else(hide_not_presented_error)?;
        grab_allowed(wait)
    })
}

/// 当前所有显示器(物理几何与缩放),供指针命中、"上次区域"钳制与长截图
/// 控制窗放置共用。
pub(crate) fn tauri_monitors(app: &AppHandle) -> Vec<MonitorGeom> {
    let Ok(monitors) = app.available_monitors() else {
        return Vec::new();
    };
    monitors
        .iter()
        .enumerate()
        .map(|(index, monitor)| {
            let size = monitor.size();
            let origin = monitor.position();
            MonitorGeom::from_physical(
                monitor
                    .name()
                    .cloned()
                    .unwrap_or_else(|| format!("monitor-{index}")),
                origin.x,
                origin.y,
                size.width,
                size.height,
                monitor.scale_factor(),
            )
        })
        .collect()
}

fn tauri_pointer_monitor(app: &AppHandle) -> Option<MonitorGeom> {
    let position = app.cursor_position().ok()?;
    let geoms = tauri_monitors(app);
    monitor_at_physical(&geoms, position.x as i32, position.y as i32)
        .cloned()
        .or_else(|| geoms.into_iter().next())
}

fn store_pixels(
    app: &AppHandle,
    frame: Frame,
    monitor: MonitorGeom,
    generation: u64,
) -> Result<(), CaptureError> {
    with_session_mut(app, |session| {
        let session = session.as_mut().ok_or_else(CaptureError::cancelled)?;
        if session.cancelled || session.generation != generation {
            return Err(CaptureError::cancelled());
        }
        session.freeze = Some(frame);
        session.overlay = None;
        session.monitor = Some(monitor);
        session.windows.clear();
        Ok(())
    })
}

fn store_freeze(
    app: &AppHandle,
    frame: Frame,
    monitor: MonitorGeom,
    windows: Vec<ListedWindow>,
    generation: u64,
) -> Result<(), CaptureError> {
    let mode = with_session(app, |session| {
        let current = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        if current.cancelled || current.generation != generation {
            return Err(CaptureError::cancelled());
        }
        Ok(current.mode)
    })?;
    // R24:Web 覆盖层能力子集随冻结帧下发;前端忽略不认识的字段。
    // R19:旧入口开关移除后,可挂载浮层固定为全开能力集。
    let capabilities = ui::OverlayCapabilities::hosted();
    let mut overlay = ui::overlay_payload(
        mode,
        &frame,
        &monitor,
        windows.clone(),
        overlay_reduced_capabilities(mode, region_native_shell()),
        capabilities,
    )?;
    overlay.record_even = mode == CaptureMode::Recording
        && crate::settings::current_recording(app).format == crate::record::RecordFormat::Mp4;
    with_session_mut(app, |session| {
        let session = session.as_mut().ok_or_else(CaptureError::cancelled)?;
        if session.cancelled || session.generation != generation {
            return Err(CaptureError::cancelled());
        }
        session.freeze = Some(frame);
        session.overlay = Some(overlay);
        session.monitor = Some(monitor);
        session.windows = windows;
        Ok(())
    })
}

pub fn overlay_frame(app: &AppHandle) -> Result<OverlayPayload, CaptureError> {
    with_session(app, |session| {
        session
            .as_ref()
            .and_then(|current| current.overlay.clone())
            // 覆盖层预创建/隐藏时无会话属正常路径:用 cancelled 让前端静默返回,
            // 不依赖文案文本(语言切换后仍稳定)。
            .ok_or_else(CaptureError::cancelled)
    })
}

pub fn preview_frame(app: &AppHandle) -> Result<PreviewPayload, CaptureError> {
    with_session(app, |session| {
        session
            .as_ref()
            .and_then(|item| item.preview.clone())
            .ok_or_else(|| CaptureError::api("error.capture.preview_missing"))
    })
}

/// 当前预览/静默会话的采集模式,供文件名模板 `{mode}` 使用。
pub fn current_capture_mode(app: &AppHandle) -> Option<CaptureMode> {
    with_session(app, |session| session.as_ref().map(|current| current.mode))
}

pub fn current_preview_frame(app: &AppHandle) -> Result<Frame, CaptureError> {
    let now = Instant::now();
    with_session_mut(app, |session| {
        let Some(current) = session.as_mut() else {
            return Err(CaptureError::api("error.capture.preview_missing"));
        };
        // A quiet-finish frame past its TTL is treated as gone, even before
        // the cleanup task runs.
        if frame_expired(current, now) {
            current.freeze = None;
            current.frame_deadline = None;
        }
        current
            .freeze
            .clone()
            .ok_or_else(|| CaptureError::api("error.capture.preview_missing"))
    })
}

pub fn mark_preview_file_written(app: &AppHandle) {
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            current.file_written = true;
        }
    });
}

/// 贴图再标注(R9):把外部帧装入预览会话并记录回写目标 label,复用全部
/// 预览命令(取帧/复制/保存/OCR)。会话被关闭或新截取覆盖时回写目标随之
/// 失效,取消不改动贴图源;进行中的截取会话不允许被覆盖。
pub fn adopt_external_frame(
    app: &AppHandle,
    frame: Frame,
    writeback: String,
) -> Result<(), CaptureError> {
    adopt_frame(app, frame, Some(writeback), "error.capture.pin_busy")
}

/// 历史再编辑:把该条图像装入预览会话,不记录贴图回写,预览仍可复制、保存和贴图。
/// 进行中的截取会话不允许被覆盖。
pub fn adopt_history_frame(app: &AppHandle, frame: Frame) -> Result<(), CaptureError> {
    adopt_frame(app, frame, None, "error.history.reedit_busy")
}

fn adopt_frame(
    app: &AppHandle,
    frame: Frame,
    writeback: Option<String>,
    busy_key: &str,
) -> Result<(), CaptureError> {
    let busy = with_session(app, |session| {
        session.as_ref().is_some_and(|current| current.busy)
    });
    if busy {
        return Err(CaptureError::api(busy_key));
    }
    let png = encode_png(&frame)?;
    let preview = ui::preview_payload(&frame, &png, ui::PreviewCopyState::Disabled, &[]);
    with_session_mut(app, |session| {
        let mut current = ActiveSession::new(CaptureMode::Region, 0, Instant::now());
        current.busy = false;
        current.preview_opened = true;
        current.freeze = Some(frame.clone());
        current.preview = Some(preview);
        current.writeback = writeback;
        *session = Some(current);
    });
    Ok(())
}

/// 当前预览会话的贴图回写目标;非再标注模式返回 None。
pub fn writeback_target(app: &AppHandle) -> Option<String> {
    with_session(app, |session| {
        session
            .as_ref()
            .and_then(|current| current.writeback.clone())
    })
}

/// MP4 在确认前把宽高向下收成偶数;其它格式保持调用方看到的矩形。
fn snap_recording_selection(
    selection: RegionSelection,
    format: crate::record::RecordFormat,
) -> RegionSelection {
    if format != crate::record::RecordFormat::Mp4 {
        return selection;
    }
    let width = selection.width & !1;
    let height = selection.height & !1;
    if width < 2 || height < 2 {
        return selection;
    }
    RegionSelection {
        width,
        height,
        ..selection
    }
}

/// 命令层区域确认:裁剪后把已定画面送进网页浮层,不打开预览、不写剪贴板。
/// R3:录屏模式下同一确认路径转为启动录制会话(Web 覆盖层路径)。
pub fn confirm_region(
    app: &AppHandle,
    selection: RegionSelection,
    annotations: Vec<Annotation>,
) -> Result<(), CaptureError> {
    let recording = with_session(app, |session| {
        session
            .as_ref()
            .is_some_and(|current| current.mode == CaptureMode::Recording)
    });
    if recording {
        let format = crate::settings::current_recording(app).format;
        let selection = snap_recording_selection(selection, format);
        let delay_ms = crate::settings::current_capture(app).delay_ms();
        if delay_ms > 0 {
            // Web 覆盖层要等这个命令返回才收起,倒计时期间选区矩形还在。
            let steps = (delay_ms / 100).max(1);
            for _ in 0..steps {
                if is_cancelled(app) {
                    return Err(CaptureError::cancelled());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        return start_recording_from_selection(app, selection, annotations, None);
    }
    finish_selection(app, selection, annotations, None)
}

/// 原生壳提交选区:不在壳上结束截图或打开预览,画面交给网页浮层工作区。
/// `pending_ocr` / `pending_qr` 让浮层打开后自动进入取字/二维码识别;
/// 提交失败时回滚这些标记。
fn commit_shell_region(
    app: &AppHandle,
    selection: RegionSelection,
    annotations: Vec<Annotation>,
    pending_ocr: bool,
    pending_qr: bool,
    generation: u64,
) -> Result<(), CaptureError> {
    if pending_ocr || pending_qr {
        with_session_mut(app, |session| {
            if let Some(current) = session.as_mut() {
                current.pending_ocr = pending_ocr;
                current.pending_qr = pending_qr;
            }
        });
    }
    let result = finish_selection(app, selection, annotations, Some(generation));
    if result.is_err() && (pending_ocr || pending_qr) {
        with_session_mut(app, |session| {
            if let Some(current) = session.as_mut() {
                current.pending_ocr = false;
                current.pending_qr = false;
            }
        });
    }
    result
}

/// 预览加载后消费一次「打开即取字」标记。
pub fn take_pending_preview_ocr(app: &AppHandle) -> bool {
    with_session_mut(app, |session| {
        session.as_mut().is_some_and(|current| {
            let pending = current.pending_ocr;
            current.pending_ocr = false;
            pending
        })
    })
}

/// 预览加载后消费一次「打开即识别二维码」标记。
pub fn take_pending_preview_qr(app: &AppHandle) -> bool {
    with_session_mut(app, |session| {
        session.as_mut().is_some_and(|current| {
            let pending = current.pending_qr;
            current.pending_qr = false;
            pending
        })
    })
}

fn finish_selection(
    app: &AppHandle,
    selection: RegionSelection,
    annotations: Vec<Annotation>,
    expected: Option<u64>,
) -> Result<(), CaptureError> {
    let (frame, annotations) = with_session(app, |session| {
        let session = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        if session.cancelled || generation_mismatch(session, expected) {
            return Err(CaptureError::cancelled());
        }
        let freeze = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("error.capture.buffer_uninitialized"))?;
        crop_selection_with_annotations(freeze, &selection, &annotations)
    })?;
    remember_selection_region(app, &selection);
    deliver_fixed_frame(app, frame, annotations, None, expected)
}

/// 选区裁剪 + 即时标注整屏坐标 → 裁剪坐标系(R21)。
/// 静默完成在 `finish_with_ttl` 内把图元合并进像素;预览完成把图元列表
/// 随干净帧下发,供预览编辑器继续编辑/撤销。
fn crop_selection_with_annotations(
    freeze: &Frame,
    selection: &RegionSelection,
    annotations: &[Annotation],
) -> Result<(Frame, Vec<Annotation>), CaptureError> {
    let frame = crop_rgba(
        freeze,
        selection.x,
        selection.y,
        selection.width,
        selection.height,
    )?;
    let translated =
        crate::annotate::translated_all(annotations, -(selection.x as f64), -(selection.y as f64));
    Ok((frame, translated))
}

/// 显示器局部物理选区 → 全局桌面物理区域(R6 记录用);0 尺寸或坐标
/// 超出 i32 时视为无有效区域。
fn global_region(
    monitor: &MonitorGeom,
    selection: &RegionSelection,
) -> Option<crate::settings::LastRegion> {
    let x = i64::from(monitor.physical_x) + i64::from(selection.x);
    let y = i64::from(monitor.physical_y) + i64::from(selection.y);
    crate::settings::LastRegion {
        x: i32::try_from(x).ok()?,
        y: i32::try_from(y).ok()?,
        width: selection.width,
        height: selection.height,
    }
    .sanitized()
}

/// 从活动会话的显示器几何与本次选区得到全局区域并写入设置;
/// 无活动会话/显示器(理论上不会发生)时跳过记录。
fn remember_selection_region(app: &AppHandle, selection: &RegionSelection) {
    let monitor = with_session(app, |session| {
        session.as_ref().and_then(|current| current.monitor.clone())
    });
    if let Some(region) = monitor
        .as_ref()
        .and_then(|monitor| global_region(monitor, selection))
    {
        crate::settings::remember_last_region(app, region);
    }
}

pub fn confirm_logical_region(app: &AppHandle, rect: LogicalRect) -> Result<(), CaptureError> {
    let physical = with_session(app, |session| {
        let session = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        let frame = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("error.capture.buffer_uninitialized"))?;
        Ok(crop_from_logical(
            frame.scale,
            rect,
            frame.width,
            frame.height,
        ))
    })?;
    confirm_region(
        app,
        RegionSelection {
            x: physical.x,
            y: physical.y,
            width: physical.width,
            height: physical.height,
        },
        Vec::new(),
    )
}

pub fn confirm_window(app: &AppHandle, window_id: String) -> Result<(), CaptureError> {
    ui::hide_window(app, ui::OVERLAY);
    require_capture_ready(app)?;
    if is_cancelled(app) {
        return Err(CaptureError::cancelled());
    }
    let cursor = cursor_mode(app);
    let (frame, outcome) = platform::capture_window_with_cursor(&window_id, cursor)?;
    note_cursor(app, outcome);
    // 窗口截图的工作区覆盖层放在窗口所在显示器:落到指针屏会与用户刚选的
    // 窗口分屏错位。列表里找不到该窗口(理论外数据)时退回会话显示器。
    let monitor = window_monitor(app, &window_id);
    deliver_fixed_frame(app, frame, Vec::new(), monitor, None)
}

/// 当前会话已列举窗口 `window_id` 所在显示器(窗口模式冻结时记录过窗口矩形)。
fn window_monitor(app: &AppHandle, window_id: &str) -> Option<MonitorGeom> {
    let listed = with_session(app, |session| {
        session.as_ref().and_then(|current| {
            current
                .windows
                .iter()
                .find(|window| window.id == window_id)
                .cloned()
        })
    })?;
    monitor_for_window(&listed, &tauri_monitors(app))
}

/// 窗口矩形中心命中的显示器;不在任何屏上时不猜。
fn monitor_for_window(window: &ListedWindow, monitors: &[MonitorGeom]) -> Option<MonitorGeom> {
    let center_x = window
        .x
        .saturating_add(i32::try_from(window.width / 2).unwrap_or(0));
    let center_y = window
        .y
        .saturating_add(i32::try_from(window.height / 2).unwrap_or(0));
    monitor_at_physical(monitors, center_x, center_y).cloned()
}

/// 完成路径的结果摘要(供动作反馈区分剪贴板成败)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinishSummary {
    pub clipboard_written: bool,
}

/// 守卫:仅活动 overlay 会话(busy 且已持冻结帧、未取消)允许静默裁剪;
/// idle-with-frame(TTL 保留帧)期间重复 invoke 直接拒绝,防止误裁旧帧。
fn allows_quiet_finish(session: &ActiveSession) -> bool {
    session.busy && !session.cancelled && session.freeze.is_some()
}

fn quiet_finish_allowed(app: &AppHandle) -> bool {
    with_session(app, |session| {
        session.as_ref().is_some_and(allows_quiet_finish)
    })
}

/// 代际校验:expected 为 None(命令层路径)时不做校验;不符即旧壳/旧会话结果。
fn generation_mismatch(session: &ActiveSession, expected: Option<u64>) -> bool {
    expected.is_some_and(|generation| session.generation != generation)
}

/// 当前会话仍是 `generation` 且未被取消:原生壳结果分发前的守卫。
fn session_matches_generation(app: &AppHandle, generation: u64) -> bool {
    with_session(app, |session| {
        session
            .as_ref()
            .is_some_and(|current| current.generation == generation && !current.cancelled)
    })
}

/// 完成路径的代际校验:`expected` 为 None(命令层)恒为 false。
fn finish_generation_stale(app: &AppHandle, expected: Option<u64>) -> bool {
    expected.is_some_and(|generation| !session_matches_generation(app, generation))
}

/// R1:滚动会话启动所需的冻结帧与显示器几何(仅同一代际的活动会话);
/// 会话已被取消/替换时返回取消错误,滚动会话不开始。
pub(crate) fn scroll_source(
    app: &AppHandle,
    generation: u64,
) -> Result<(Frame, MonitorGeom), CaptureError> {
    with_session(app, |session| {
        let current = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        if current.cancelled || current.generation != generation {
            return Err(CaptureError::cancelled());
        }
        let frame = current
            .freeze
            .clone()
            .ok_or_else(|| CaptureError::invalid_buffer("error.capture.buffer_uninitialized"))?;
        let monitor = current
            .monitor
            .clone()
            .ok_or_else(|| CaptureError::api("error.capture.no_monitor"))?;
        Ok((frame, monitor))
    })
}

/// R1:滚动会话是否仍属于当前代际(被替换/取消后立即停止抓取且不产出)。
pub(crate) fn scroll_session_alive(app: &AppHandle, generation: u64) -> bool {
    session_matches_generation(app, generation)
}

/// 区域选区里启动的滚动完成仍挂在 `Region` 上。进入 `deliver_fixed_frame`
/// 之前把这一代改为 `LongCapture`，`record_capture` 才写成 `long`。
/// 代际不符或已取消不改；托盘直达的 `LongCapture` 重复设置无变化，
/// 普通区域完成不经过这里。
fn mark_scroll_capture_mode(session: &mut Option<ActiveSession>, generation: u64) {
    if let Some(current) = session.as_mut() {
        if current.generation == generation && !current.cancelled {
            current.mode = CaptureMode::LongCapture;
        }
    }
}

/// R1:滚动结果走现有预览完成路径(预览、历史与「上次区域」规则);选区
/// 标注先平移到拼接结果坐标系。代际不符时返回取消错误且不产生输出。
pub(crate) fn finish_scroll_frame(
    app: &AppHandle,
    frame: Frame,
    annotations: Vec<Annotation>,
    selection: &RegionSelection,
    expected: u64,
) -> Result<(), CaptureError> {
    if !session_matches_generation(app, expected) {
        return Err(CaptureError::cancelled());
    }
    let translated =
        crate::annotate::translated_all(&annotations, -(selection.x as f64), -(selection.y as f64));
    with_session_mut(app, |session| mark_scroll_capture_mode(session, expected));
    deliver_fixed_frame(app, frame, translated, None, Some(expected))?;
    remember_selection_region(app, selection);
    Ok(())
}

/// R1:滚动会话取消/内容未变化等不产出路径的统一收尾:释放槽位、恢复
/// 产品表面并广播取消;不写剪贴板、历史或磁盘。重复调用是 no-op。
pub(crate) fn cancel_scroll_session(app: &AppHandle, expected: u64) {
    let _ = cancel_internal(app, Some(expected));
}

/// 带守卫的静默完成入口:命令层 `finish_region_with` 与原生选区壳的操作条/
/// 菜单动作共用;`expected` 为壳启动时的会话代际(命令层为 None)。
/// 完成后按动作给出反馈;复制在剪贴板写入失败时提示失败而非「已复制」。
pub async fn finish_region_with(
    app: &AppHandle,
    selection: RegionSelection,
    annotations: Vec<Annotation>,
    action: QuietAction,
    expected: Option<u64>,
) -> Result<(), CaptureError> {
    let handle = app.clone();
    let finish = tauri::async_runtime::spawn_blocking(move || {
        if !quiet_finish_allowed(&handle) {
            return Err(CaptureError::api("error.capture.region_missing"));
        }
        finish_region_quiet(&handle, selection, annotations, DEFAULT_FRAME_TTL, expected)
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))??;
    match action {
        QuietAction::Copy => {
            if finish.clipboard_written {
                ui::show_toast_key(app, "toast.copied");
            } else {
                ui::show_toast_key(app, "toast.copy_failed");
            }
        }
        // 保存/取字/贴图沿用命令层的统一动作分发(保存对话框、离线 OCR、toast)。
        other => super::run_quiet_action(app, other).await,
    }
    Ok(())
}

/// Quiet completion of an explicit region: the cropped frame still reaches the
/// clipboard, no preview opens, and the session goes idle-with-frame for the
/// configured TTL so save/ocr/copy/pin can reuse the retained frame.
pub fn finish_region_quiet(
    app: &AppHandle,
    selection: RegionSelection,
    annotations: Vec<Annotation>,
    ttl: Duration,
    expected: Option<u64>,
) -> Result<FinishSummary, CaptureError> {
    let (frame, annotations) = with_session(app, |session| {
        let session = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        if session.cancelled || generation_mismatch(session, expected) {
            return Err(CaptureError::cancelled());
        }
        let freeze = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("error.capture.buffer_uninitialized"))?;
        crop_selection_with_annotations(freeze, &selection, &annotations)
    })?;
    ui::hide_window(app, ui::OVERLAY);
    // 显式动作(操作条/菜单复制等)不受 autoCopy 开关影响,始终写剪贴板;
    // 即时标注在 `finish_with_ttl` 内先合并进像素再执行动作。
    let summary = finish_with_ttl(
        app,
        frame,
        annotations,
        FinishDisposition::Quiet,
        ttl,
        true,
        expected,
    )?;
    // R6:静默完成同属成功完成的区域截图,同样刷新"上次区域"。
    remember_selection_region(app, &selection);
    Ok(summary)
}

pub fn cancel(app: &AppHandle) -> Result<CancelOutcome, CaptureError> {
    cancel_internal(app, None)
}

/// 取消受理(纯状态迁移):标记 Cancelling 并返回 (generation, 待恢复表面)。
/// 已受理或代际不符(旧壳)返回 None,调用方不重复清理。
fn take_cancel_request(
    session: &mut Option<ActiveSession>,
    expected: Option<u64>,
) -> Option<(u64, Vec<RecordedSurface>)> {
    let current = session.as_mut()?;
    if current.cancelled || generation_mismatch(current, expected) {
        return None;
    }
    current.cancelled = true;
    current.cancel_requested_at = Some(Instant::now());
    Some((current.generation, current.hide.restore_on_cancel()))
}

/// 释放已取消会话:仅当槽位仍是同一 generation 的 Cancelling 会话时才移除,
/// 因此旧清理任务不会删除后来占位的新会话。
fn release_cancelled(session: &mut Option<ActiveSession>, generation: u64) -> bool {
    if session
        .as_ref()
        .is_some_and(|current| current.generation == generation && current.is_cancelling())
    {
        *session = None;
        true
    } else {
        false
    }
}

/// 取消受理(进入 Cancelling 阶段)的会话层入口。
fn accept_cancel(app: &AppHandle, expected: Option<u64>) -> Option<(u64, Vec<RecordedSurface>)> {
    with_session_mut(app, |session| take_cancel_request(session, expected))
}

/// 取消清理完成:隐藏会话窗、恢复产品表面、广播取消并释放槽位。
/// `close_shell` 仅在壳可能仍在泵消息时为 true(托盘取消/看门狗);
/// 壳已经返回 Cancelled 后再关会误伤下一次热键新建的选区窗。
fn finish_cancel(
    app: &AppHandle,
    generation: u64,
    restore: Vec<RecordedSurface>,
    close_shell: bool,
) {
    log::info!("capture cancel id={generation}");
    if close_shell {
        close_active_native_shell();
    }
    for label in ui::session_window_labels() {
        ui::hide_window(app, label);
    }
    for surface in restore {
        ui::show_window(app, &surface.label);
    }
    let _ = app.emit("capture-cancelled", ());
    let newer_capture = with_session(app, |session| {
        session
            .as_ref()
            .is_some_and(|current| current.generation != generation && current.busy)
    });
    if !newer_capture {
        crate::pin::restore_after_capture(app);
    }
    release_cancelled_session(app, generation);
}

/// 仅在槽位仍是同一 Cancelling 会话时释放它;新会话绝不被旧清理任务删除。
fn release_cancelled_session(app: &AppHandle, generation: u64) -> bool {
    with_session_mut(app, |session| release_cancelled(session, generation))
}

/// 壳还在泵消息时再次热键会把本代际标成 Cancelling;壳返回后必须收尾,
/// 否则槽位一直占着,新截取只能等到 30s 看门狗。
fn cleanup_cancelled_generation(app: &AppHandle, generation: u64) {
    let restore = with_session_mut(app, |session| {
        session.as_ref().and_then(|current| {
            if current.generation == generation && current.cancelled {
                Some(current.hide.restore_on_cancel())
            } else {
                None
            }
        })
    });
    if let Some(restore) = restore {
        finish_cancel(app, generation, restore, false);
    }
}

fn cancel_internal(app: &AppHandle, expected: Option<u64>) -> Result<CancelOutcome, CaptureError> {
    if let Some((generation, restore)) = accept_cancel(app, expected) {
        finish_cancel(app, generation, restore, true);
    }
    // Cancel never writes the clipboard, a file, or a preview window.
    Ok(CancelOutcome::clean())
}

/// 已定画面的去向(R2):浮层能挂上工作区动作时停在冻结帧工作区覆盖层
/// (取字、复制、保存、贴图、进一步编辑都在这里);挂不上时退回预览。
/// 二维码单独走预览:结果是一段文本,按图片大小的窗口看,不铺满屏幕。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRoute {
    Overlay,
    Preview,
}

/// 取字和二维码都打开按图片大小的预览，不进入全屏冻结层。
pub fn recognition_surface(pending_ocr: bool, pending_qr: bool) -> WorkspaceRoute {
    if pending_ocr || pending_qr {
        WorkspaceRoute::Preview
    } else {
        WorkspaceRoute::Overlay
    }
}

pub fn workspace_route(hosted: bool) -> WorkspaceRoute {
    if hosted {
        WorkspaceRoute::Overlay
    } else {
        WorkspaceRoute::Preview
    }
}

/// 画面已经定下来:默认停在冻结帧工作区覆盖层,不再直接打开编辑页;不写
/// 剪贴板。覆盖层挂不上工作区动作(或没有落点显示器)时说明缺失并退回预览。
fn deliver_fixed_frame(
    app: &AppHandle,
    frame: Frame,
    annotations: Vec<Annotation>,
    monitor: Option<MonitorGeom>,
    expected: Option<u64>,
) -> Result<(), CaptureError> {
    if is_cancelled(app) || finish_generation_stale(app, expected) {
        return Err(CaptureError::cancelled());
    }
    let (mode, session_monitor, pending_ocr, pending_qr) = with_session(app, |session| {
        let current = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        if current.cancelled || generation_mismatch(current, expected) {
            return Err(CaptureError::cancelled());
        }
        Ok((
            current.mode,
            current.monitor.clone(),
            current.pending_ocr,
            current.pending_qr,
        ))
    })?;
    let monitor = monitor.or(session_monitor);
    // 取字和二维码不进全屏冻结层。标记留在会话里,预览打开后消费并开始识别。
    if recognition_surface(pending_ocr, pending_qr) == WorkspaceRoute::Preview {
        return open_workspace_preview(app, frame, annotations, expected);
    }
    // R19:旧入口开关移除后工作区动作固定全开;分支保留给确实挂不上的宿主。
    let capabilities = ui::OverlayCapabilities::hosted();
    if workspace_route(capabilities.workspace_actions) == WorkspaceRoute::Preview {
        ui::show_toast_key(app, "overlay.notice.workspace_unavailable");
        return open_workspace_preview(app, frame, annotations, expected);
    }
    let Some(monitor) = monitor else {
        ui::show_toast_key(app, "overlay.notice.workspace_unavailable");
        return open_workspace_preview(app, frame, annotations, expected);
    };
    let mut overlay = ui::overlay_payload(
        mode,
        &frame,
        &monitor,
        Vec::new(),
        overlay_reduced_capabilities(mode, region_native_shell()),
        capabilities,
    )?;
    overlay.fixed = true;
    overlay.annotations = annotations;
    overlay.pending_ocr = pending_ocr;
    overlay.pending_qr = pending_qr;
    with_session_mut(app, |session| {
        let current = session.as_mut().ok_or_else(CaptureError::cancelled)?;
        if current.cancelled || generation_mismatch(current, expected) {
            return Err(CaptureError::cancelled());
        }
        current.freeze = Some(frame.clone());
        current.overlay = Some(overlay);
        current.monitor = Some(monitor.clone());
        current.windows.clear();
        current.preview = None;
        current.preview_opened = false;
        Ok(())
    })?;
    if ui::open_overlay(app, &monitor).is_err() {
        ui::show_toast_key(app, "overlay.notice.workspace_unavailable");
        return open_workspace_preview(app, frame, annotations_from_overlay(app), expected);
    }
    Ok(())
}

/// 工作区覆盖层已装载的标注;打开失败退回预览时不能丢掉已画内容。
fn annotations_from_overlay(app: &AppHandle) -> Vec<Annotation> {
    with_session(app, |session| {
        session
            .as_ref()
            .and_then(|current| current.overlay.as_ref())
            .map(|overlay| overlay.annotations.clone())
            .unwrap_or_default()
    })
}

/// 进一步编辑,或已定画面进入编辑页:打开含当前标注的预览,不写剪贴板。
fn open_workspace_preview(
    app: &AppHandle,
    frame: Frame,
    annotations: Vec<Annotation>,
    expected: Option<u64>,
) -> Result<(), CaptureError> {
    match finish_with_ttl(
        app,
        frame,
        annotations,
        FinishDisposition::Preview,
        DEFAULT_FRAME_TTL,
        false,
        expected,
    ) {
        Ok(_) => Ok(()),
        Err(error) => {
            if !error.is_cancelled() {
                let _ = ui::open_error(app, &error);
            }
            Err(error)
        }
    }
}

fn workspace_frame(app: &AppHandle) -> Result<Frame, CaptureError> {
    with_session(app, |session| {
        session
            .as_ref()
            .and_then(|current| current.freeze.clone())
            .ok_or_else(|| CaptureError::api("error.capture.preview_missing"))
    })
}

/// 复制/保存/贴图成功后关闭浮层并提示。失败不得调用本函数。
pub fn complete_workspace(
    app: &AppHandle,
    kind: &str,
    name: Option<&str>,
) -> Result<(), CaptureError> {
    let frame = workspace_frame(app)?;
    // 保存对话框会临时露出预览窗;成功离开时把它和浮层一起收起。
    ui::hide_window(app, ui::PREVIEW);
    ui::hide_window(app, ui::OVERLAY);
    let mode = current_capture_mode(app);
    crate::history::record_capture(app, frame, mode);
    crate::pin::restore_after_capture(app);
    with_session_mut(app, |session| {
        *session = None;
    });
    match kind {
        "save" => {
            let label = name.unwrap_or("cropmark");
            ui::show_toast_key_params(app, "toast.saved", &[("name", label)]);
        }
        "pin" => ui::show_toast_key(app, "toast.pinned"),
        _ => ui::show_toast_key(app, "toast.copied"),
    }
    Ok(())
}

/// 全屏工作区上的「取字」改去预览:按图片大小打开,并自动开始识别。
pub fn preview_workspace_ocr(
    app: &AppHandle,
    annotations: Vec<Annotation>,
) -> Result<(), CaptureError> {
    let frame = workspace_frame(app)?;
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            current.pending_ocr = true;
            current.pending_qr = false;
        }
    });
    open_workspace_preview(app, frame, annotations, None)
}

/// 全屏工作区上的「识别二维码」改去预览:按图片大小打开,并自动开始识别。
pub fn preview_workspace_qr(
    app: &AppHandle,
    annotations: Vec<Annotation>,
) -> Result<(), CaptureError> {
    let frame = workspace_frame(app)?;
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            current.pending_ocr = false;
            current.pending_qr = true;
        }
    });
    open_workspace_preview(app, frame, annotations, None)
}

/// 进一步编辑:关闭浮层,打开含当前标注的预览,不写剪贴板。R2/R4:覆盖层
/// 打开时已消费壳上的取字/二维码标记,预览不再重复自动识别。
pub fn edit_workspace_further(
    app: &AppHandle,
    annotations: Vec<Annotation>,
) -> Result<(), CaptureError> {
    let frame = workspace_frame(app)?;
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            current.pending_ocr = false;
            current.pending_qr = false;
        }
    });
    open_workspace_preview(app, frame, annotations, None)
}

/// 浮层挂不上工作区动作:说明缺失并打开预览,不写剪贴板。
pub fn fallback_workspace_preview(
    app: &AppHandle,
    annotations: Vec<Annotation>,
) -> Result<(), CaptureError> {
    ui::show_toast_key(app, "overlay.notice.workspace_unavailable");
    let frame = workspace_frame(app)?;
    open_workspace_preview(app, frame, annotations, None)
}

/// 静默复制的剪贴板帧:对副本做与 `compose_output` 相同的美化。
/// 原帧继续留给冻结画面、预览载荷、历史、贴图和 OCR。
fn quiet_clipboard_frame(
    frame: &Frame,
    options: &crate::beautify::BeautifyOptions,
) -> Result<Frame, CaptureError> {
    crate::beautify::compose_frame(frame.clone(), true, options)
}

fn write_finish_clipboard(
    app: &AppHandle,
    frame: &Frame,
    plain_png: &[u8],
) -> Result<(), CaptureError> {
    if !crate::settings::current_export(app).apply_beautify {
        return clipboard::copy_frame_with_png(frame, plain_png);
    }
    let options = crate::settings::current_export(app).beautify;
    let output = quiet_clipboard_frame(frame, &options)?;
    let png = encode_png(&output)?;
    clipboard::copy_frame_with_png(&output, &png)
}

fn finish_with_ttl(
    app: &AppHandle,
    frame: Frame,
    annotations: Vec<Annotation>,
    disposition: FinishDisposition,
    frame_ttl: Duration,
    auto_copy: bool,
    expected: Option<u64>,
) -> Result<FinishSummary, CaptureError> {
    let started = Instant::now();
    if is_cancelled(app) || finish_generation_stale(app, expected) {
        return Err(CaptureError::cancelled());
    }
    // R21:静默完成(复制/保存/贴图/取字/静默 Enter)先把即时标注合并进
    // 像素,输出与所见一致;单个图元失败(字体缺失等)跳过而不阻断完成。
    // 预览完成保留干净帧 + 图元列表,供继续编辑。
    let frame = if disposition == FinishDisposition::Quiet {
        rasterize_lenient(&frame, &annotations)
    } else {
        frame
    };
    let png = encode_png(&frame)?;
    let encoded_at = started.elapsed();
    if is_cancelled(app) || finish_generation_stale(app, expected) {
        return Err(CaptureError::cancelled());
    }
    // autoCopy 关闭时不写剪贴板;显式复制路径不受此开关影响。
    // 美化只写剪贴板副本。冻结帧、预览载荷、历史、贴图和 OCR 继续用未美化帧,
    // 静默保存因此只在写盘时美化一次。
    let clipboard_error = if auto_copy {
        write_finish_clipboard(app, &frame, &png).err()
    } else {
        None
    };
    let copied_at = started.elapsed();
    let clipboard_written = auto_copy && clipboard_error.is_none();
    let copy_state = match (auto_copy, clipboard_error.is_none()) {
        (false, _) => ui::PreviewCopyState::Disabled,
        (true, true) => ui::PreviewCopyState::Copied,
        (true, false) => ui::PreviewCopyState::Failed,
    };
    let preview_annotations: &[Annotation] = if disposition == FinishDisposition::Preview {
        &annotations
    } else {
        &[]
    };
    let preview = ui::preview_payload(&frame, &png, copy_state, preview_annotations);
    // 编码/剪贴板写入期间会话可能已被 stale 重置替换:替换后不再提交状态、
    // 不打开预览,旧帧不得污染新会话(ADR-16)。
    if is_cancelled(app) || finish_generation_stale(app, expected) {
        return Err(CaptureError::cancelled());
    }
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            if clipboard_written {
                current.clipboard.commit_success();
            }
            current.freeze = Some(frame.clone());
            current.preview = Some(preview);
            // R6:新预览载荷对应新坐标系,变换历史随之作废。
            current.preview_transforms.reset();
            current.file_written = false;
        }
    });
    ui::hide_window(app, ui::OVERLAY);
    ui::hide_window(app, ui::DELAY);
    ui::hide_window(app, ui::ERROR);
    let quiet_deadline = match disposition {
        FinishDisposition::Preview => {
            ui::open_preview(app, &frame)?;
            with_session_mut(app, |session| {
                if let Some(current) = session.as_mut() {
                    finish_transition(
                        current,
                        FinishDisposition::Preview,
                        Instant::now(),
                        frame_ttl,
                    );
                }
            });
            None
        }
        FinishDisposition::Quiet => {
            let mut deadline = None;
            with_session_mut(app, |session| {
                if let Some(current) = session.as_mut() {
                    finish_transition(current, FinishDisposition::Quiet, Instant::now(), frame_ttl);
                    deadline = current.frame_deadline;
                }
            });
            deadline
        }
    };
    if let Some(deadline) = quiet_deadline {
        spawn_frame_ttl_cleanup(app.clone(), deadline);
    }
    if capture_timing_enabled() {
        eprintln!(
            "Cropmark capture {}x{}: PNG={:?}, clipboard={:?}, preview={:?}, total={:?}",
            frame.width,
            frame.height,
            encoded_at,
            copied_at - encoded_at,
            started.elapsed() - copied_at,
            started.elapsed()
        );
    }
    if let Some(ref error) = clipboard_error {
        set_last_error(app, Some(error.clone()));
        let _ = ui::open_error(app, error);
    }
    // R2:history.enabled 时把最终帧写入本地历史;编码、缩略图与索引写入
    // 全部在 spawn_blocking 内,不阻塞完成路径。模式随这条记录写入索引(R7)。
    let mode = current_capture_mode(app);
    let (width, height, bytes) = (frame.width, frame.height, png.len());
    crate::history::record_capture(app, frame, mode);
    crate::pin::restore_after_capture(app);
    if take_cursor_unavailable(app) {
        ui::show_toast_key(app, "toast.cursor_unavailable");
    }
    log::info!(
        "capture end size={width}x{height} bytes={bytes} disposition={}",
        match disposition {
            FinishDisposition::Preview => "preview",
            FinishDisposition::Quiet => "quiet",
        }
    );
    Ok(FinishSummary { clipboard_written })
}

/// Session state transition at the end of a finish. Split out so the quiet
/// lifecycle (idle-with-frame + TTL) is unit-testable without an app handle.
fn finish_transition(
    session: &mut ActiveSession,
    disposition: FinishDisposition,
    now: Instant,
    frame_ttl: Duration,
) {
    session.busy = false;
    session.cancelled = false;
    session.file_written = false;
    match disposition {
        FinishDisposition::Preview => {
            session.preview_opened = true;
            session.frame_deadline = None;
        }
        FinishDisposition::Quiet => {
            session.preview = None;
            session.frame_deadline = Some(now + frame_ttl);
        }
    }
}

fn frame_expired(session: &ActiveSession, now: Instant) -> bool {
    session
        .frame_deadline
        .is_some_and(|deadline| now >= deadline)
}

/// Only the exact idle quiet session whose deadline elapsed may be released,
/// so a newer busy session is never dropped by a stale cleanup task.
fn should_release_frame(session: &ActiveSession, deadline: Instant) -> bool {
    !session.busy && session.frame_deadline == Some(deadline)
}

fn spawn_frame_ttl_cleanup(app: AppHandle, deadline: Instant) {
    tauri::async_runtime::spawn(async move {
        let wait = deadline.saturating_duration_since(Instant::now());
        let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(wait)).await;
        with_session_mut(&app, |session| {
            if session
                .as_ref()
                .is_some_and(|current| should_release_frame(current, deadline))
            {
                *session = None;
            }
        });
    });
}

pub fn delay_state(app: &AppHandle) -> DelayPayload {
    with_session(app, |session| match session.as_ref() {
        Some(current) => DelayPayload {
            delay_ms: current.delay_ms,
            mode: current.mode,
        },
        None => DelayPayload {
            delay_ms: 0,
            mode: CaptureMode::Region,
        },
    })
}

pub fn last_error(app: &AppHandle) -> Option<CaptureError> {
    let runtime = app.state::<CaptureRuntime>();
    let error = lock(&runtime.last_error).clone();
    // 按当前语言重解析(语言切换后已打开的错误窗显示新语言)。
    error.map(|error| error.localized())
}

fn finish_error(app: &AppHandle, error: CaptureError) -> Result<(), CaptureError> {
    let restore = with_session(app, |session| {
        session
            .as_ref()
            .map(|current| current.hide.restore_on_cancel())
            .unwrap_or_default()
    });
    for label in ui::session_window_labels() {
        ui::hide_window(app, label);
    }
    for surface in restore {
        ui::show_window(app, &surface.label);
    }
    with_session_mut(app, |session| *session = None);
    set_last_error(app, Some(error.clone()));
    log::warn!("capture failed kind={:?}", error.kind);
    let opened = ui::open_error(app, &error);
    if capture_timing_enabled() {
        match &opened {
            Ok(()) => eprintln!("Cropmark capture: error window shown kind={:?}", error.kind),
            Err(open_error) => eprintln!(
                "Cropmark capture: error window failed kind={:?} error={open_error:?}",
                error.kind
            ),
        }
    }
    opened
}

fn is_cancelled(app: &AppHandle) -> bool {
    with_session(app, |session| {
        session
            .as_ref()
            .map(|current| current.cancelled)
            .unwrap_or(true)
    })
}

pub fn close_preview(app: &AppHandle) {
    ui::hide_window(app, ui::PREVIEW);
    with_session_mut(app, |session| *session = None);
    crate::front::demote_if_idle(app);
}

pub fn close_error(app: &AppHandle) {
    // 仅隐藏:error 窗是预创建复用的 webview。
    ui::hide_window(app, ui::ERROR);
    crate::front::demote_if_idle(app);
}

/// R6:裁剪最小边长(物理像素):更小的选区分不出内容,直接拒绝而不是产出
/// 一条像素级别的截图。
pub const MIN_CROP_EDGE: u32 = 8;

/// R6:预览旋转方向(前端命令参数)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RotationDirection {
    Left,
    Right,
}

/// R6:一次预览几何变换。帧变换复用共享原语,坐标重映射共用同一几何。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewTransformOp {
    RotateCw,
    RotateCcw,
    Crop(PreviewCrop),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewCrop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl PreviewTransformOp {
    fn geometry(self, frame: &Frame) -> FrameTransform {
        match self {
            Self::RotateCw => FrameTransform::RotateCw {
                height: frame.height as f64,
            },
            Self::RotateCcw => FrameTransform::RotateCcw {
                width: frame.width as f64,
            },
            Self::Crop(crop) => FrameTransform::Crop {
                dx: -(crop.x as f64),
                dy: -(crop.y as f64),
            },
        }
    }

    fn apply_frame(self, frame: &Frame) -> Result<Frame, CaptureError> {
        match self {
            Self::RotateCw => Ok(rotate_frame_cw(frame)),
            Self::RotateCcw => Ok(rotate_frame_ccw(frame)),
            Self::Crop(crop) => crop_rgba(frame, crop.x, crop.y, crop.width, crop.height),
        }
    }
}

/// 裁剪拒绝边界:小于最小边长或越出当前帧时给出本地化说明且不产生副作用。
fn validate_crop(frame: &Frame, crop: &PreviewCrop) -> Result<(), CaptureError> {
    if crop.width < MIN_CROP_EDGE || crop.height < MIN_CROP_EDGE {
        return Err(CaptureError::api_detail(
            "error.preview.crop_too_small",
            &MIN_CROP_EDGE.to_string(),
        ));
    }
    let right = u64::from(crop.x) + u64::from(crop.width);
    let bottom = u64::from(crop.y) + u64::from(crop.height);
    if right > u64::from(frame.width) || bottom > u64::from(frame.height) {
        return Err(CaptureError::api("error.preview.crop_out_of_bounds"));
    }
    Ok(())
}

/// R6:一次重基后的完整预览状态(帧 + 标注 + 取字 + 撤销/重做可用性)。
#[derive(Debug, Clone)]
struct PreviewRebase {
    frame: Frame,
    annotations: Vec<Annotation>,
    ocr: Option<OcrDocument>,
    can_undo: bool,
    can_redo: bool,
}

impl PreviewRebase {
    fn outcome(&self) -> PreviewTransformOutcome {
        PreviewTransformOutcome {
            width: self.frame.width,
            height: self.frame.height,
            annotations: self.annotations.clone(),
            ocr: self.ocr.clone(),
            can_undo: self.can_undo,
            can_redo: self.can_redo,
        }
    }
}

/// R6:预览旋转/裁剪命令返回:重基后的帧尺寸、标注与取字结果,以及变换
/// 历史的撤销/重做可用性。前端据此 `setAnnotations` + `setDocument` 并重拉像素。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewTransformOutcome {
    pub width: u32,
    pub height: u32,
    pub annotations: Vec<Annotation>,
    pub ocr: Option<OcrDocument>,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Debug, Clone)]
struct PreviewTransformEntry {
    op: PreviewTransformOp,
    /// 变换前的标注/取字状态:撤销到该位置时原样恢复(含用户此前编辑)。
    annotations_before: Vec<Annotation>,
    ocr_before: Option<OcrDocument>,
    annotations_after: Vec<Annotation>,
    ocr_after: Option<OcrDocument>,
}

/// R6:预览会话级变换快照。帧由基准帧依序重放(旋转/裁剪均无损),标注与
/// 取字结果随每步保存;撤销/重做回放同一序列,前端标注栈按 `setAnnotations`
/// 语义清空,两层历史拼成统一 LIFO(见 `rotate_preview`/`crop_preview` 等命令)。
#[derive(Debug, Clone, Default)]
struct PreviewTransforms {
    base: Option<Frame>,
    entries: Vec<PreviewTransformEntry>,
    position: usize,
}

impl PreviewTransforms {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn can_undo(&self) -> bool {
        self.position > 0
    }

    fn can_redo(&self) -> bool {
        self.position < self.entries.len()
    }

    fn frame_at(&self, position: usize) -> Result<Frame, CaptureError> {
        let mut frame = self
            .base
            .clone()
            .ok_or_else(|| CaptureError::api("error.capture.preview_missing"))?;
        for entry in self.entries.iter().take(position) {
            frame = entry.op.apply_frame(&frame)?;
        }
        Ok(frame)
    }

    /// 组装某一位置的重基结果:帧由重放得到,标注/取字由调用方按撤销或
    /// 重做方向给出(两步之间的用户编辑只存在于下一步的 before 快照)。
    fn rebase(
        &self,
        position: usize,
        annotations: Vec<Annotation>,
        ocr: Option<OcrDocument>,
    ) -> Result<PreviewRebase, CaptureError> {
        Ok(PreviewRebase {
            frame: self.frame_at(position)?,
            annotations,
            ocr,
            can_undo: position > 0,
            can_redo: position < self.entries.len(),
        })
    }

    /// 记录并应用一次变换:首次变换记录重放基准帧;撤销分支上的新变换丢弃
    /// 之后的 redo 记录。拒绝(裁剪越界/过小)发生在写入历史之前。
    fn push(
        &mut self,
        current: &Frame,
        op: PreviewTransformOp,
        annotations: Vec<Annotation>,
        ocr: Option<OcrDocument>,
    ) -> Result<PreviewRebase, CaptureError> {
        let before = if self.base.is_some() {
            self.frame_at(self.position)?
        } else {
            current.clone()
        };
        if let PreviewTransformOp::Crop(crop) = op {
            validate_crop(&before, &crop)?;
        }
        if self.base.is_none() {
            self.base = Some(current.clone());
        }
        let transform = op.geometry(&before);
        let annotations_after = transformed_all(&annotations, transform);
        let ocr_after = ocr
            .as_ref()
            .map(|doc| crate::ocr::remap_document(doc, transform));
        self.entries.truncate(self.position);
        self.entries.push(PreviewTransformEntry {
            op,
            annotations_before: annotations,
            ocr_before: ocr,
            annotations_after,
            ocr_after,
        });
        self.position += 1;
        self.rebase(
            self.position,
            self.entries[self.position - 1].annotations_after.clone(),
            self.entries[self.position - 1].ocr_after.clone(),
        )
    }

    /// 撤销第 `position` 步:回到该步应用前的标注/取字状态(含该步之前用户
    /// 的编辑),帧回到重放 `position - 1` 步的结果。
    fn undo(&mut self) -> Result<PreviewRebase, CaptureError> {
        if !self.can_undo() {
            return Err(CaptureError::api("error.preview.no_transform"));
        }
        self.position -= 1;
        let entry = &self.entries[self.position];
        self.rebase(
            self.position,
            entry.annotations_before.clone(),
            entry.ocr_before.clone(),
        )
    }

    /// 重做第 `position + 1` 步:恢复该步应用后的标注/取字状态。
    fn redo(&mut self) -> Result<PreviewRebase, CaptureError> {
        if !self.can_redo() {
            return Err(CaptureError::api("error.preview.no_transform"));
        }
        let entry = &self.entries[self.position];
        let annotations = entry.annotations_after.clone();
        let ocr = entry.ocr_after.clone();
        self.position += 1;
        self.rebase(self.position, annotations, ocr)
    }
}

/// 变换后的预览载荷与取字结果回写:仅当会话仍是此次变换的代际且未取消时
/// 才替换预览载荷,避免旧变换污染新会话。
fn commit_preview_rebase(
    app: &AppHandle,
    generation: u64,
    rebase: &PreviewRebase,
) -> Result<(), CaptureError> {
    let png = encode_png(&rebase.frame)?;
    crate::ocr::set_last_document(app, rebase.ocr.clone());
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            if current.generation == generation && !current.cancelled {
                current.preview = Some(ui::preview_payload(
                    &rebase.frame,
                    &png,
                    ui::PreviewCopyState::Disabled,
                    &[],
                ));
            }
        }
    });
    Ok(())
}

/// R6:在当前预览会话上应用一次旋转/裁剪:重基帧、标注与取字结果,并写回
/// 会话冻结帧与预览载荷。无预览会话时拒绝,不作用于其它窗口。
fn apply_preview_transform(
    app: &AppHandle,
    op: PreviewTransformOp,
    annotations: Vec<Annotation>,
) -> Result<PreviewRebase, CaptureError> {
    // 取字结果与帧同源:先取最近一次识别(进行中的识别持锁完成后返回),
    // 变换后写回重映射结果,保证 copy_ocr_* 与图上高亮使用新帧坐标。
    let ocr = crate::ocr::last_document(app);
    let (rebase, generation) = with_session_mut(app, |session| {
        let current = session
            .as_mut()
            .ok_or_else(|| CaptureError::api("error.capture.preview_missing"))?;
        if current.cancelled || current.preview.is_none() {
            return Err(CaptureError::api("error.capture.preview_missing"));
        }
        let frame = current
            .freeze
            .clone()
            .ok_or_else(|| CaptureError::api("error.capture.preview_missing"))?;
        let rebase = current
            .preview_transforms
            .push(&frame, op, annotations, ocr)?;
        current.freeze = Some(rebase.frame.clone());
        Ok((rebase, current.generation))
    })?;
    commit_preview_rebase(app, generation, &rebase)?;
    Ok(rebase)
}

/// R6:撤销/重放一次预览变换;无预览会话或历史到头时拒绝。
fn step_preview_transform(app: &AppHandle, forward: bool) -> Result<PreviewRebase, CaptureError> {
    let (rebase, generation) = with_session_mut(app, |session| {
        let current = session
            .as_mut()
            .ok_or_else(|| CaptureError::api("error.capture.preview_missing"))?;
        if current.cancelled || current.preview.is_none() {
            return Err(CaptureError::api("error.capture.preview_missing"));
        }
        let rebase = if forward {
            current.preview_transforms.redo()?
        } else {
            current.preview_transforms.undo()?
        };
        current.freeze = Some(rebase.frame.clone());
        Ok((rebase, current.generation))
    })?;
    commit_preview_rebase(app, generation, &rebase)?;
    Ok(rebase)
}

/// 变换命令的公共异步外壳:PNG 编码与等待进行中的取字识别都不允许占用
/// 主线程(与贴图/识别命令同一约定),统一放到阻塞线程池执行。
async fn run_preview_transform(
    app: AppHandle,
    op: PreviewTransformOp,
    annotations: Vec<Annotation>,
) -> Result<PreviewTransformOutcome, CaptureError> {
    tauri::async_runtime::spawn_blocking(move || {
        apply_preview_transform(&app, op, annotations).map(|rebase| rebase.outcome())
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

async fn run_preview_history(
    app: AppHandle,
    forward: bool,
) -> Result<PreviewTransformOutcome, CaptureError> {
    tauri::async_runtime::spawn_blocking(move || {
        step_preview_transform(&app, forward).map(|rebase| rebase.outcome())
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

/// R6:预览左/右旋转 90°(作用于当前冻结帧与已放置标注)。
#[tauri::command]
pub async fn rotate_preview(
    app: AppHandle,
    direction: RotationDirection,
    annotations: Vec<Annotation>,
) -> Result<PreviewTransformOutcome, CaptureError> {
    let op = match direction {
        RotationDirection::Left => PreviewTransformOp::RotateCcw,
        RotationDirection::Right => PreviewTransformOp::RotateCw,
    };
    run_preview_transform(app, op, annotations).await
}

/// R6:裁剪当前预览帧到拖选区域;小于最小边长或越界拒绝且无副作用。
#[tauri::command]
pub async fn crop_preview(
    app: AppHandle,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    annotations: Vec<Annotation>,
) -> Result<PreviewTransformOutcome, CaptureError> {
    run_preview_transform(
        app,
        PreviewTransformOp::Crop(PreviewCrop {
            x,
            y,
            width,
            height,
        }),
        annotations,
    )
    .await
}

/// R6:撤销上一次预览旋转/裁剪(前端标注栈为空时由 Ctrl+Z 触发)。
#[tauri::command]
pub async fn undo_preview_transform(
    app: AppHandle,
) -> Result<PreviewTransformOutcome, CaptureError> {
    run_preview_history(app, false).await
}

/// R6:重做上一次被撤销的预览旋转/裁剪。
#[tauri::command]
pub async fn redo_preview_transform(
    app: AppHandle,
) -> Result<PreviewTransformOutcome, CaptureError> {
    run_preview_history(app, true).await
}

fn with_session<R>(app: &AppHandle, f: impl FnOnce(&Option<ActiveSession>) -> R) -> R {
    let runtime = app.state::<CaptureRuntime>();
    let guard = lock(&runtime.inner);
    f(&guard)
}

fn with_session_mut<R>(app: &AppHandle, f: impl FnOnce(&mut Option<ActiveSession>) -> R) -> R {
    let runtime = app.state::<CaptureRuntime>();
    let mut guard = lock(&runtime.inner);
    f(&mut guard)
}

fn set_last_error(app: &AppHandle, error: Option<CaptureError>) {
    let runtime = app.state::<CaptureRuntime>();
    *lock(&runtime.last_error) = error;
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::error::CaptureErrorKind;
    use crate::capture::hide::{
        grab_allowed, plan_delay, session_steps, HideWait, RecordedSurface, SessionStep,
        SurfaceKind,
    };
    use crate::i18n;

    fn hide_before_capture_steps(mode: CaptureMode, delay_ms: u64) -> Vec<SessionStep> {
        session_steps(!matches!(mode, CaptureMode::Fullscreen), delay_ms)
    }

    #[test]
    fn multi_monitor_off_keeps_pointer_screen_only() {
        assert_eq!(
            effective_fullscreen_target(false, FullscreenTarget::All),
            FullscreenTarget::Pointer
        );
        assert_eq!(
            effective_fullscreen_target(false, FullscreenTarget::Monitor("a|0|0|1|1".into())),
            FullscreenTarget::Pointer
        );
        assert_eq!(
            effective_fullscreen_target(true, FullscreenTarget::All),
            FullscreenTarget::All
        );
    }

    #[test]
    fn window_capture_workspace_follows_the_window_monitor() {
        // 窗口截图的工作区覆盖层放在窗口所在显示器,而不是指针落点屏。
        let monitors = vec![
            MonitorGeom::from_physical("left", 0, 0, 1920, 1080, 1.0),
            MonitorGeom::from_physical("right", 1920, 0, 1280, 1024, 1.0),
        ];
        let window = |x: i32, y: i32| ListedWindow {
            id: "w1".into(),
            title: "Notes".into(),
            pid: 7,
            x,
            y,
            width: 400,
            height: 300,
            visible: true,
            owner_is_self: false,
        };
        assert_eq!(
            monitor_for_window(&window(2100, 400), &monitors)
                .unwrap()
                .id,
            "right"
        );
        assert_eq!(
            monitor_for_window(&window(100, 100), &monitors).unwrap().id,
            "left"
        );
        // 窗口跨屏时按中心归类;不在任何屏上时不猜。
        assert!(monitor_for_window(&window(-5000, -5000), &monitors).is_none());
    }

    #[test]
    fn region_window_fullscreen_and_tray_delay_hide_before_capture() {
        for mode in CaptureMode::ALL {
            for delay_ms in [0_u64, 3000] {
                let plan = plan_delay(delay_ms);
                assert!(plan.hide_before_delay);
                assert!(!plan.overlay_during_delay);
                let steps = hide_before_capture_steps(mode, delay_ms);
                let hide = steps
                    .iter()
                    .position(|&step| step == SessionStep::Hide)
                    .expect("hide-before-capture");
                let presented = steps
                    .iter()
                    .position(|&step| step == SessionStep::WaitPresented)
                    .expect("wait presented");
                let pixels = steps
                    .iter()
                    .position(|&step| step == SessionStep::CapturePixels)
                    .expect("capture pixels");
                assert!(hide < presented);
                assert!(presented < pixels);
                if delay_ms > 0 {
                    let delay = steps
                        .iter()
                        .position(|&step| step == SessionStep::DelayWithoutOverlay)
                        .expect("tray delay without overlay");
                    assert!(presented < delay);
                    assert!(delay < pixels);
                }
                match mode {
                    CaptureMode::Region | CaptureMode::Window => {
                        assert_eq!(
                            steps.last().copied(),
                            Some(SessionStep::ShowOverlayOnFreeze)
                        );
                    }
                    CaptureMode::Fullscreen => {
                        assert!(!steps.contains(&SessionStep::ShowOverlayOnFreeze));
                        assert_eq!(workspace_route(true), WorkspaceRoute::Overlay);
                    }
                    // R1:长截图无热键,不进入 `CaptureMode::ALL`。
                    CaptureMode::LongCapture => unreachable!("LongCapture is not a hotkey mode"),
                    // R3:录屏无热键,不进入 `CaptureMode::ALL`。
                    CaptureMode::Recording => unreachable!("Recording is not a hotkey mode"),
                }
            }
        }
    }

    #[test]
    fn region_session_hides_before_pixels_and_draws_overlay_on_freeze() {
        let steps = session_steps(true, 0);
        assert_eq!(
            steps,
            [
                SessionStep::RecordSurfaces,
                SessionStep::Hide,
                SessionStep::WaitPresented,
                SessionStep::CapturePixels,
                SessionStep::ShowOverlayOnFreeze,
            ]
        );
    }

    /// R3:录屏模式的选区走区域壳(冻结帧上覆盖层);普通区域模式的录屏动作
    /// 只由设置开关控制;录屏模式不出现即时标注/取字/静默动作与长截图。
    #[test]
    fn recording_selection_flags_gate_the_action_and_reduce_recording_mode_actions() {
        let off = recording_selection_flags(CaptureMode::Region, false, false);
        assert!(!off.recording);
        assert!(!off.long_capture);
        assert!(off.inline_annotation && off.ocr_entry && off.toolbar_copy && off.toolbar_save);

        let on = recording_selection_flags(CaptureMode::Region, true, true);
        assert!(on.recording);
        assert!(on.long_capture && on.inline_annotation && on.ocr_entry);

        let recording = recording_selection_flags(CaptureMode::Recording, false, true);
        assert!(recording.recording);
        assert!(!recording.long_capture);
        assert!(!recording.inline_annotation);
        assert!(!recording.ocr_entry && !recording.pin_entry);
        assert!(!recording.toolbar_copy && !recording.toolbar_save && !recording.toolbar_pin);

        // R1:长截图模式的裁剪保持不变,且不带录屏入口。
        let long = recording_selection_flags(CaptureMode::LongCapture, true, false);
        assert!(long.long_capture);
        assert!(!long.recording);
        assert!(!long.inline_annotation && !long.ocr_entry);

        // 录屏模式与普通区域模式一样在冻结帧上挂覆盖层。
        let steps = session_steps(true, 0);
        assert_eq!(
            steps.last().copied(),
            Some(SessionStep::ShowOverlayOnFreeze)
        );
        assert!(!CaptureMode::ALL.contains(&CaptureMode::Recording));
    }

    #[test]
    fn fullscreen_delivers_workspace_overlay() {
        let steps = session_steps(false, 0);
        assert!(!steps.contains(&SessionStep::ShowOverlayOnFreeze));
        assert_eq!(workspace_route(true), WorkspaceRoute::Overlay);
        assert_ne!(workspace_route(true), WorkspaceRoute::Preview);
    }

    #[test]
    fn qr_recognition_opens_preview_instead_of_the_fullscreen_overlay() {
        assert_eq!(
            recognition_surface(false, true),
            WorkspaceRoute::Preview
        );
        assert_eq!(
            recognition_surface(true, false),
            WorkspaceRoute::Preview
        );
        assert_eq!(
            recognition_surface(true, true),
            WorkspaceRoute::Preview
        );
        assert_eq!(
            recognition_surface(false, false),
            WorkspaceRoute::Overlay
        );
    }

    #[test]
    fn cancel_then_occupy_starts_a_new_region_capture() {
        let now = Instant::now();
        let mut slot = Some(ActiveSession::new(CaptureMode::Region, 0, now));
        let generation = slot.as_ref().unwrap().generation;
        assert_eq!(
            begin_decision(slot.as_ref(), now),
            BeginDecision::IgnoreBusy
        );
        assert!(take_cancel_request(&mut slot, Some(generation)).is_some());
        assert!(release_cancelled(&mut slot, generation));
        assert!(slot.is_none());
        assert_eq!(begin_decision(slot.as_ref(), now), BeginDecision::Begin);
        assert!(occupy_session(&mut slot, CaptureMode::Region, 0, now));
        assert!(slot.as_ref().unwrap().busy);
        assert_ne!(slot.as_ref().unwrap().generation, generation);
    }

    #[test]
    fn leftover_cancel_flag_on_idle_session_does_not_block_begin() {
        let now = Instant::now();
        let mut session = ActiveSession::new(CaptureMode::Region, 0, now);
        session.busy = false;
        session.cancelled = true;
        assert_eq!(begin_decision(Some(&session), now), BeginDecision::Begin);
    }

    #[test]
    fn overlapping_busy_capture_is_marked_cancelling() {
        let now = Instant::now();
        let mut slot = Some(ActiveSession::new(CaptureMode::Region, 0, now));
        assert_eq!(
            begin_decision(slot.as_ref(), now + Duration::from_millis(40)),
            BeginDecision::IgnoreBusy
        );
        assert!(take_cancel_request(&mut slot, None).is_some());
        assert!(slot.as_ref().unwrap().cancelled);
        assert_eq!(
            begin_decision(slot.as_ref(), now + Duration::from_millis(40)),
            BeginDecision::WaitForCancelClearance
        );
    }

    #[test]
    fn pending_preview_recognition_flags_are_independent() {
        let mut session = ActiveSession::new(CaptureMode::Region, 0, Instant::now());
        session.busy = false;
        session.pending_ocr = true;
        session.pending_qr = true;
        assert!(session.pending_ocr);
        assert!(session.pending_qr);
        session.pending_ocr = false;
        assert!(!session.pending_ocr);
        assert!(session.pending_qr, "消费取字标记不得清掉二维码标记");
        session.pending_qr = false;
        assert!(!session.pending_qr);
    }

    /// R2:静默动作不再包含取字;壳上的「取字」走工作区覆盖层而不是 Quiet 复制。
    /// 旧前端/旧壳若仍发送 ocr 会被拒绝,不再有先写 PNG 再写文本的路径。
    #[test]
    fn quiet_actions_never_include_text_recognition() {
        assert_eq!(
            serde_json::from_str::<QuietAction>("\"copy\"").expect("copy stays quiet"),
            QuietAction::Copy
        );
        assert_eq!(
            serde_json::from_str::<QuietAction>("\"save\"").expect("save stays quiet"),
            QuietAction::Save
        );
        assert_eq!(
            serde_json::from_str::<QuietAction>("\"pin\"").expect("pin stays quiet"),
            QuietAction::Pin
        );
        assert!(serde_json::from_str::<QuietAction>("\"ocr\"").is_err());
    }

    #[test]
    fn busy_session_ignores_overlapping_capture() {
        let now = Instant::now();
        let mut slot = Some(ActiveSession::new(CaptureMode::Region, 3000, now));
        let started_at = slot.as_ref().unwrap().started_at;
        assert_eq!(
            begin_decision(slot.as_ref(), now + Duration::from_millis(40)),
            BeginDecision::IgnoreBusy
        );
        assert!(!occupy_session(
            &mut slot,
            CaptureMode::Fullscreen,
            0,
            now + Duration::from_millis(40)
        ));
        let session = slot.expect("busy session kept");
        assert!(session.busy);
        assert_eq!(session.mode, CaptureMode::Region);
        assert_eq!(session.delay_ms, 3000);
        assert_eq!(session.started_at, started_at);
    }

    #[test]
    fn idle_or_absent_session_allows_new_capture() {
        let now = Instant::now();
        let mut slot = None;
        assert!(occupy_session(&mut slot, CaptureMode::Window, 0, now));
        assert_eq!(slot.as_ref().unwrap().mode, CaptureMode::Window);

        let mut idle = ActiveSession::new(CaptureMode::Region, 0, now);
        idle.busy = false;
        idle.frame_deadline = Some(now + DEFAULT_FRAME_TTL);
        let mut slot = Some(idle);
        assert!(occupy_session(
            &mut slot,
            CaptureMode::Fullscreen,
            3000,
            now
        ));
        let session = slot.unwrap();
        assert!(session.busy);
        assert_eq!(session.mode, CaptureMode::Fullscreen);
        assert_eq!(session.delay_ms, 3000);
        assert!(session.frame_deadline.is_none());
    }

    #[test]
    fn stale_busy_session_is_reset_so_capture_can_begin() {
        let started = Instant::now();
        let now = started + STALE_SESSION_TIMEOUT + Duration::from_secs(1);
        let mut slot = Some(ActiveSession::new(CaptureMode::Region, 0, started));
        assert_eq!(
            begin_decision(slot.as_ref(), now),
            BeginDecision::ResetStaleThenBegin
        );
        assert!(occupy_session(&mut slot, CaptureMode::Window, 3000, now));
        let session = slot.unwrap();
        assert!(session.busy);
        assert_eq!(session.mode, CaptureMode::Window);
        assert_eq!(session.delay_ms, 3000);
        assert_eq!(session.started_at, now);
    }

    #[test]
    fn cancelling_session_waits_then_releases_and_restores_product_surfaces() {
        let mut session = ActiveSession::new(CaptureMode::Region, 0, Instant::now());
        session.clipboard.commit_success();
        session.file_written = true;
        session.preview_opened = true;
        session.freeze = Some(Frame {
            width: 2,
            height: 2,
            rgba: vec![0; 16],
            scale: 1.0,
        });
        session.hide = HideWait::record(vec![
            RecordedSurface {
                label: "preview".into(),
                kind: SurfaceKind::Preview,
                was_visible: true,
            },
            RecordedSurface {
                label: "settings".into(),
                kind: SurfaceKind::Settings,
                was_visible: true,
            },
            RecordedSurface {
                label: "overlay".into(),
                kind: SurfaceKind::Overlay,
                was_visible: true,
            },
        ]);
        session.hide.request_hide();
        let generation = session.generation;
        let mut slot = Some(session);
        let (cancelled_generation, restore) =
            take_cancel_request(&mut slot, None).expect("had session");
        assert_eq!(cancelled_generation, generation);
        // Cancelling 阶段:会话仍占槽位、不可完成,但恢复清单已就绪。
        let current = slot.as_ref().expect("kept while cleaning");
        assert!(current.is_cancelling());
        assert!(!allows_quiet_finish(current));
        let labels: Vec<_> = restore
            .iter()
            .map(|surface| surface.label.as_str())
            .collect();
        assert_eq!(labels, ["preview", "settings"]);
        // 取消本身不写剪贴板/文件/预览(clean outcome 恒为全 false)。
        let outcome = CancelOutcome::clean();
        assert!(!outcome.clipboard_written);
        assert!(!outcome.file_written);
        assert!(!outcome.preview_opened);
        // 重复受理被忽略;清理完成后才释放,重复释放为 no-op。
        assert!(take_cancel_request(&mut slot, None).is_none());
        assert!(release_cancelled(&mut slot, generation));
        assert!(slot.is_none());
        assert!(!release_cancelled(&mut slot, generation));
    }

    #[test]
    fn cancel_reentry_repeats_five_times_without_losing_the_trigger() {
        // "取消→立即区域截图"连续 5 次:清理窗口内的触发短时等待,清理完成即开始。
        let now = Instant::now();
        let mut slot: Option<ActiveSession> = None;
        for _ in 0..5 {
            assert!(occupy_session(&mut slot, CaptureMode::Region, 0, now));
            let generation = slot.as_ref().unwrap().generation;
            assert_eq!(
                begin_decision(slot.as_ref(), now),
                BeginDecision::IgnoreBusy,
                "进行中的截取仍静默忽略重叠触发"
            );
            let (cancelled, _restore) =
                take_cancel_request(&mut slot, Some(generation)).expect("cancel accepted");
            assert_eq!(cancelled, generation);
            // 清理窗口:新触发等待而不是被吞。
            assert_eq!(
                begin_decision(slot.as_ref(), now + Duration::from_millis(20)),
                BeginDecision::WaitForCancelClearance
            );
            // 清理完成 → 槽位释放 → 下一次触发直接开始。
            assert!(release_cancelled(&mut slot, generation));
            assert_eq!(begin_decision(slot.as_ref(), now), BeginDecision::Begin);
        }
    }

    #[test]
    fn stuck_cancelling_session_is_reset_by_the_watchdog() {
        let now = Instant::now();
        let mut slot = Some(ActiveSession::new(CaptureMode::Region, 0, now));
        take_cancel_request(&mut slot, None).expect("cancel accepted");
        let later = now + STALE_SESSION_TIMEOUT + Duration::from_secs(1);
        assert_eq!(
            begin_decision(slot.as_ref(), later),
            BeginDecision::ResetStaleThenBegin
        );
    }

    #[test]
    fn stale_shell_cancel_and_cleanup_never_touch_the_new_session() {
        let now = Instant::now();
        let mut slot = Some(ActiveSession::new(CaptureMode::Region, 0, now));
        let old_generation = slot.as_ref().unwrap().generation;
        // 看门狗已用新会话替换旧会话。
        let replacement = ActiveSession::new(CaptureMode::Window, 3000, now);
        let new_generation = replacement.generation;
        slot = Some(replacement);
        // 旧壳的取消请求(携带旧代际)被拒绝,新会话不受影响。
        assert!(take_cancel_request(&mut slot, Some(old_generation)).is_none());
        assert!(!slot.as_ref().unwrap().cancelled);
        // 旧清理任务也不能释放新会话。
        assert!(!release_cancelled(&mut slot, old_generation));
        assert!(slot.is_some());
        // 新会话自己的取消可受理,并且只释放它。
        assert!(take_cancel_request(&mut slot, Some(new_generation)).is_some());
        assert!(release_cancelled(&mut slot, new_generation));
        assert!(slot.is_none());
    }

    fn listed_window(id: &str) -> ListedWindow {
        ListedWindow {
            id: id.into(),
            title: "Notes".into(),
            pid: 11,
            x: 0,
            y: 0,
            width: 800,
            height: 600,
            visible: true,
            owner_is_self: false,
        }
    }

    #[test]
    fn empty_or_wayland_window_list_is_unavailable_not_blank_preview() {
        let empty = windows_for_window_mode(Ok(Vec::new())).unwrap_err();
        assert_eq!(empty.kind, CaptureErrorKind::Unavailable);
        assert_eq!(empty.message, i18n::t(EMPTY_WINDOW_LIST_KEY));
        // 提示必须给出替代路径,而不是只报"没有窗口"(R13 保持明确)。
        assert!(empty.message.contains("区域"));
        assert!(empty.message.contains("全屏"));

        let wayland = windows_for_window_mode(Err(CaptureError::unavailable(
            "error.linux.portal_window_list",
        )))
        .unwrap_err();
        assert_eq!(wayland.kind, CaptureErrorKind::Unavailable);
        assert!(wayland.message.contains("无法列出窗口"));
        assert!(wayland.message.contains("区域"));
        assert!(wayland.message.contains("全屏"));
        assert!(!wayland.message.is_empty());

        let listed = windows_for_window_mode(Ok(vec![listed_window("w1")])).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "w1");
    }

    #[test]
    fn wayland_web_overlay_reports_reduced_capabilities() {
        // 区域模式 + 无原生壳(Wayland/Linux 网页路径):需要能力说明。
        assert!(overlay_reduced_capabilities(CaptureMode::Region, false));
        // 区域模式 + 原生壳(Windows/macOS/X11):原生能力可用,不展示说明。
        assert!(!overlay_reduced_capabilities(CaptureMode::Region, true));
        // 窗口模式的 Web 覆盖层不提供这些能力,但不得把 Wayland 说明误报给用户。
        assert!(!overlay_reduced_capabilities(CaptureMode::Window, false));
        assert!(!overlay_reduced_capabilities(CaptureMode::Window, true));
        assert!(!overlay_reduced_capabilities(
            CaptureMode::Fullscreen,
            false
        ));
        assert!(!overlay_reduced_capabilities(CaptureMode::Fullscreen, true));
    }

    #[test]
    fn snap_fallback_notice_covers_unavailable_but_not_window_only() {
        use crate::capture::snap::SnapCapability;
        // 完整能力与窗口级降级都无提示:后者由平台授权/降级流程说明。
        assert_eq!(snap_fallback_notice_key(SnapCapability::Full), None);
        assert_eq!(
            snap_fallback_notice_key(SnapCapability::WindowOnly {
                reason_key: "error.capture.snap_control_unavailable",
            }),
            None
        );
        // 完全不可用时沿用现有本地化能力说明机制。
        assert_eq!(
            snap_fallback_notice_key(SnapCapability::Unavailable {
                reason_key: "error.capture.snap_unavailable",
            }),
            Some("error.capture.snap_unavailable")
        );
    }

    #[test]
    fn grab_is_blocked_until_hide_wait_commits() {
        let mut wait = HideWait::record(vec![RecordedSurface {
            label: "preview".into(),
            kind: SurfaceKind::Preview,
            was_visible: true,
        }]);
        wait.request_hide();
        assert!(grab_allowed(&wait).is_err());
        wait.commit_presented(true, true).unwrap();
        assert!(grab_allowed(&wait).is_ok());
    }

    fn active_session() -> ActiveSession {
        ActiveSession::new(CaptureMode::Region, 0, Instant::now())
    }

    fn solid_frame(width: u32, height: u32, color: [u8; 4]) -> Frame {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            rgba.extend_from_slice(&color);
        }
        Frame {
            width,
            height,
            rgba,
            scale: 1.0,
        }
    }

    #[test]
    fn quiet_copy_beautifies_a_frame_copy_when_export_beautify_is_on() {
        let frame = solid_frame(8, 8, [12, 34, 56, 255]);
        let options = crate::beautify::BeautifyOptions {
            preset: "paper".into(),
            padding: 4,
            radius: 0,
            shadow: false,
        };
        let output = quiet_clipboard_frame(&frame, &options).unwrap();
        assert_eq!((frame.width, frame.height), (8, 8));
        assert_eq!(&frame.rgba[0..4], &[12, 34, 56, 255]);
        assert_eq!((output.width, output.height), (16, 16));
        assert_eq!(&output.rgba[0..4], &[0xF4, 0xF1, 0xEA, 255]);
        let last = output.rgba.len() - 4;
        assert_eq!(&output.rgba[last..], &[0xF4, 0xF1, 0xEA, 255]);
        let origin = ((4 * output.width + 4) * 4) as usize;
        assert_eq!(&output.rgba[origin..origin + 4], &[12, 34, 56, 255]);
        let png = encode_png(&output).unwrap();
        let decoded = crate::capture::buffer::decode_png(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (16, 16));
        assert_eq!(&decoded.rgba[0..4], &[0xF4, 0xF1, 0xEA, 255]);
        assert_eq!(&decoded.rgba[last..], &[0xF4, 0xF1, 0xEA, 255]);
        // 写盘会对保留的原帧美化一次,结果应与剪贴板副本一致;
        // 再对剪贴板帧美化会继续变大,所以保存不能吃这份副本。
        let saved_once = crate::beautify::compose_frame(frame.clone(), true, &options).unwrap();
        assert_eq!(saved_once.rgba, output.rgba);
        let saved_twice = quiet_clipboard_frame(&output, &options).unwrap();
        assert!(saved_twice.width > output.width);
        assert!(saved_twice.height > output.height);
    }

    #[test]
    fn quiet_finish_keeps_frame_with_ttl_and_goes_idle() {
        let mut session = active_session();
        session.freeze = Some(Frame {
            width: 2,
            height: 2,
            rgba: vec![0; 16],
            scale: 1.0,
        });
        let now = Instant::now();
        finish_transition(
            &mut session,
            FinishDisposition::Quiet,
            now,
            DEFAULT_FRAME_TTL,
        );
        assert!(!session.busy);
        assert!(session.preview.is_none());
        assert!(session.freeze.is_some());
        assert_eq!(session.frame_deadline, Some(now + DEFAULT_FRAME_TTL));
        assert!(!frame_expired(
            &session,
            now + DEFAULT_FRAME_TTL - Duration::from_millis(1)
        ));
        assert!(frame_expired(&session, now + DEFAULT_FRAME_TTL));
    }

    #[test]
    fn preview_finish_keeps_frame_without_deadline() {
        let mut session = active_session();
        let now = Instant::now();
        finish_transition(
            &mut session,
            FinishDisposition::Preview,
            now,
            DEFAULT_FRAME_TTL,
        );
        assert!(!session.busy);
        assert!(session.preview_opened);
        assert_eq!(session.frame_deadline, None);
        assert!(!frame_expired(
            &session,
            now + DEFAULT_FRAME_TTL + Duration::from_secs(1)
        ));
    }

    #[test]
    fn quiet_finish_guard_allows_only_active_overlay_session() {
        let mut session = active_session();
        session.freeze = Some(Frame {
            width: 2,
            height: 2,
            rgba: vec![0; 16],
            scale: 1.0,
        });
        // 活动 overlay 会话:允许静默裁剪。
        assert!(allows_quiet_finish(&session));
        // idle-with-frame(静默完成后的 TTL 保留帧):拒绝,防误裁旧帧。
        session.busy = false;
        session.frame_deadline = Some(Instant::now() + DEFAULT_FRAME_TTL);
        assert!(!allows_quiet_finish(&session));
        // 取消中的会话同样拒绝。
        session.busy = true;
        session.cancelled = true;
        assert!(!allows_quiet_finish(&session));
        // 尚未持冻结帧:拒绝。
        session.cancelled = false;
        session.freeze = None;
        assert!(!allows_quiet_finish(&session));
    }

    #[test]
    fn ttl_release_matches_only_idle_quiet_session_deadline() {
        let mut session = active_session();
        let now = Instant::now();
        finish_transition(
            &mut session,
            FinishDisposition::Quiet,
            now,
            Duration::from_millis(50),
        );
        let deadline = session.frame_deadline.expect("quiet sets a deadline");
        assert!(should_release_frame(&session, deadline));
        // A new capture in flight must not be released by a stale cleanup task.
        session.busy = true;
        assert!(!should_release_frame(&session, deadline));
        // A different deadline (a newer quiet finish) is not ours to release.
        session.busy = false;
        assert!(!should_release_frame(
            &session,
            deadline + Duration::from_secs(1)
        ));
        assert!(!should_release_frame(
            &session,
            deadline - Duration::from_secs(1)
        ));
    }

    #[test]
    fn fixed_capture_stays_on_workspace_overlay_and_ignores_finish_settings() {
        let capture = crate::settings::CaptureSettings::default();
        assert_eq!(capture.delay_seconds, 0);
        let hosted = ui::OverlayCapabilities::hosted();
        assert!(hosted.workspace_actions);
        assert_eq!(workspace_route(true), WorkspaceRoute::Overlay);
        assert_eq!(
            workspace_route(hosted.workspace_actions),
            WorkspaceRoute::Overlay
        );
        let saved = serde_json::from_str::<crate::settings::CaptureSettings>(
            r#"{"delaySeconds":3,"autoCopy":true,"finishAction":"preview"}"#,
        )
        .expect("old finish fields are ignored");
        assert_eq!(saved.delay_seconds, 3);
        let quiet = serde_json::from_str::<crate::settings::CaptureSettings>(
            r#"{"delaySeconds":1,"autoCopy":true,"finishAction":"quiet"}"#,
        )
        .expect("old quiet finish is ignored");
        assert_eq!(quiet.delay_seconds, 1);
        assert_eq!(workspace_route(true), WorkspaceRoute::Overlay);
    }

    #[test]
    fn unhosted_workspace_falls_back_to_preview() {
        let unhosted = ui::OverlayCapabilities::unhosted();
        assert!(!unhosted.workspace_actions);
        assert_eq!(
            workspace_route(unhosted.workspace_actions),
            WorkspaceRoute::Preview
        );
        // 挂得上的宿主不走预览回退。
        assert_eq!(workspace_route(false), WorkspaceRoute::Preview);
        assert_eq!(workspace_route(true), WorkspaceRoute::Overlay);
        assert_ne!(workspace_route(true), workspace_route(false));
    }

    #[test]
    fn monitor_local_selection_maps_to_global_physical_region() {
        let monitor = MonitorGeom::from_physical("left", -1920, 0, 1920, 1080, 1.0);
        let selection = RegionSelection {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };
        assert_eq!(
            global_region(&monitor, &selection),
            Some(crate::settings::LastRegion {
                x: -1910,
                y: 20,
                width: 300,
                height: 200
            })
        );
        // 0 尺寸选区不产生记录。
        let zero = RegionSelection {
            x: 0,
            y: 0,
            width: 0,
            height: 5,
        };
        assert_eq!(global_region(&monitor, &zero), None);
    }

    #[test]
    fn last_region_crop_stays_inside_target_monitor_frame() {
        let monitor = MonitorGeom::from_physical("right", 1920, 0, 2560, 1440, 2.0);
        let region = crate::settings::LastRegion {
            x: 2000,
            y: 100,
            width: 400,
            height: 300,
        };
        assert_eq!(local_crop(&monitor, &region), Some((80, 100, 400, 300)));

        // 区域完全落在显示器之外:不得产生负坐标,调用方给提示而非越界裁剪。
        let outside = crate::settings::LastRegion {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert_eq!(local_crop(&monitor, &outside), None);

        // 区域超出右/下边缘:同样拒绝(计划阶段已钳制,这里是防御检查)。
        let overflow = crate::settings::LastRegion {
            x: 1920,
            y: 1400,
            width: 5000,
            height: 100,
        };
        assert_eq!(local_crop(&monitor, &overflow), None);
    }

    #[test]
    fn last_region_plan_picks_monitor_with_largest_overlap() {
        let monitors = [
            MonitorGeom::from_physical("left", -1920, 0, 1920, 1080, 1.0),
            MonitorGeom::from_physical("right", 0, 0, 2560, 1440, 1.0),
        ];
        let rects: Vec<crate::settings::LastRegion> = monitors
            .iter()
            .map(|monitor| crate::settings::LastRegion {
                x: monitor.physical_x,
                y: monitor.physical_y,
                width: monitor.physical_width,
                height: monitor.physical_height,
            })
            .collect();
        // 跨屏区域:重叠更大的右屏胜出并裁剪进右屏。
        let straddling = crate::settings::LastRegion {
            x: -300,
            y: 100,
            width: 800,
            height: 400,
        };
        let (index, clamped) = straddling.clamp_to_monitors(&rects).expect("has overlap");
        assert_eq!(index, 1);
        assert_eq!(
            clamped,
            crate::settings::LastRegion {
                x: 0,
                y: 100,
                width: 500,
                height: 400
            }
        );
        assert!(local_crop(&monitors[index], &clamped).is_some());
    }

    #[test]
    fn last_region_toast_messages_are_actionable() {
        assert!(LastRegionPlanError::Missing.toast().contains("上次区域"));
        assert!(LastRegionPlanError::OutOfRange.toast().contains("显示范围"));
    }

    #[test]
    fn crop_selection_translates_inline_annotations_into_crop_coordinates() {
        use crate::annotate::Annotation;
        let width = 100u32;
        let height = 80u32;
        let freeze = Frame {
            width,
            height,
            rgba: vec![10; (width * height * 4) as usize],
            scale: 1.0,
        };
        let selection = RegionSelection {
            x: 20,
            y: 10,
            width: 30,
            height: 20,
        };
        let annotations = vec![Annotation::Rect {
            x: 25.0,
            y: 15.0,
            width: 10.0,
            height: 8.0,
            color: "#e11d48".into(),
            stroke_width: None,
        }];
        let (cropped, translated) =
            crop_selection_with_annotations(&freeze, &selection, &annotations).unwrap();
        assert_eq!((cropped.width, cropped.height), (30, 20));
        match &translated[0] {
            Annotation::Rect { x, y, .. } => assert_eq!((*x, *y), (5.0, 5.0)),
            other => panic!("expected rect, got {other:?}"),
        }
        // 静默完成路径:图元合并进裁剪像素,输出与所见一致。
        let merged = rasterize_lenient(&cropped, &translated);
        assert_eq!((merged.width, merged.height), (30, 20));
        assert_ne!(merged.rgba, cropped.rgba, "annotation must change pixels");
    }

    #[test]
    fn crop_selection_without_annotations_keeps_plain_pixels() {
        let freeze = Frame {
            width: 40,
            height: 30,
            rgba: vec![7; 40 * 30 * 4],
            scale: 1.0,
        };
        let selection = RegionSelection {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        };
        let (cropped, translated) =
            crop_selection_with_annotations(&freeze, &selection, &[]).unwrap();
        assert!(translated.is_empty());
        assert_eq!(cropped.rgba, vec![7; 10 * 10 * 4]);
    }

    #[test]
    fn region_started_scroll_history_mode_is_long() {
        let mut region = Some(ActiveSession::new(CaptureMode::Region, 0, Instant::now()));
        let generation = region.as_ref().unwrap().generation;
        mark_scroll_capture_mode(&mut region, generation);
        let mode = region.as_ref().unwrap().mode;
        assert_eq!(mode, CaptureMode::LongCapture);
        assert_eq!(crate::export::capture_mode_token(mode), "long");

        // 托盘直接以 LongCapture 开始：再标一次仍是 long。
        let mut direct = Some(ActiveSession::new(
            CaptureMode::LongCapture,
            0,
            Instant::now(),
        ));
        let direct_generation = direct.as_ref().unwrap().generation;
        mark_scroll_capture_mode(&mut direct, direct_generation);
        assert_eq!(direct.as_ref().unwrap().mode, CaptureMode::LongCapture);
        assert_eq!(
            crate::export::capture_mode_token(direct.as_ref().unwrap().mode),
            "long"
        );

        // 另一代的区域/窗口会话不被这次滚动完成改写。
        let mut untouched = Some(ActiveSession::new(CaptureMode::Window, 0, Instant::now()));
        mark_scroll_capture_mode(&mut untouched, generation);
        assert_eq!(untouched.as_ref().unwrap().mode, CaptureMode::Window);
        assert_eq!(
            crate::export::capture_mode_token(untouched.as_ref().unwrap().mode),
            "window"
        );
        let ordinary = ActiveSession::new(CaptureMode::Region, 0, Instant::now());
        assert_eq!(ordinary.mode, CaptureMode::Region);
        assert_eq!(crate::export::capture_mode_token(ordinary.mode), "region");

        // 已取消的同一代不进入完成路径，模式保持原样。
        let mut cancelled = Some(ActiveSession::new(CaptureMode::Region, 0, Instant::now()));
        cancelled.as_mut().unwrap().cancelled = true;
        let cancelled_generation = cancelled.as_ref().unwrap().generation;
        mark_scroll_capture_mode(&mut cancelled, cancelled_generation);
        assert_eq!(cancelled.as_ref().unwrap().mode, CaptureMode::Region);

        let mut absent = None;
        mark_scroll_capture_mode(&mut absent, generation);
        assert!(absent.is_none());
    }

    fn pattern_frame(width: u32, height: u32) -> Frame {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                bytes.extend_from_slice(&[
                    (x * 7) as u8,
                    (y * 11) as u8,
                    (x * 3 + y * 5) as u8,
                    255,
                ]);
            }
        }
        Frame {
            width,
            height,
            rgba: bytes,
            scale: 1.0,
        }
    }

    fn rect_annotation(x: f64, y: f64, width: f64, height: f64) -> Annotation {
        Annotation::Rect {
            x,
            y,
            width,
            height,
            color: "#e11d48".into(),
            stroke_width: None,
        }
    }

    fn ocr_document() -> OcrDocument {
        OcrDocument {
            spans: vec![crate::ocr::hit::TextSpan {
                text: "甲".into(),
                x: 1.0,
                y: 0.5,
                width: 3.0,
                height: 1.0,
            }],
            full_text: "甲".into(),
        }
    }

    #[test]
    fn preview_rotation_rebases_frame_annotations_and_ocr() {
        let base = pattern_frame(8, 4);
        let mut history = PreviewTransforms::default();
        let annotations = vec![rect_annotation(1.0, 1.0, 2.0, 2.0)];
        let ocr = ocr_document();

        let rotated = history
            .push(
                &base,
                PreviewTransformOp::RotateCw,
                annotations.clone(),
                Some(ocr.clone()),
            )
            .unwrap();
        assert_eq!((rotated.frame.width, rotated.frame.height), (4, 8));
        assert_eq!(rotated.frame.rgba, rotate_frame_cw(&base).rgba);
        assert!(rotated.can_undo && !rotated.can_redo);
        match &rotated.annotations[0] {
            Annotation::Rect {
                x,
                y,
                width,
                height,
                ..
            } => assert_eq!((*x, *y, *width, *height), (4.0 - 1.0 - 2.0, 1.0, 2.0, 2.0)),
            other => panic!("expected rect, got {other:?}"),
        }
        let span = &rotated.ocr.as_ref().unwrap().spans[0];
        // 源帧高 4:span (1, 0.5, 3, 1) → (4 - 0.5 - 1, 1, 1, 3)。
        assert_eq!(
            (span.x, span.y, span.width, span.height),
            (2.5, 1.0, 1.0, 3.0)
        );
        assert_eq!(rotated.ocr.as_ref().unwrap().full_text, "甲");

        // 逆时针把方向转回来:帧、标注与取字都回到基准。
        let back = history
            .push(
                &rotated.frame,
                PreviewTransformOp::RotateCcw,
                rotated.annotations.clone(),
                rotated.ocr.clone(),
            )
            .unwrap();
        assert_eq!(back.frame.rgba, base.rgba);
        assert_eq!(back.annotations, annotations);
        assert_eq!(back.ocr, Some(ocr));
    }

    #[test]
    fn preview_crop_rebases_and_undo_redo_round_trips_steps() {
        let base = pattern_frame(12, 10);
        let mut history = PreviewTransforms::default();
        let first = vec![rect_annotation(0.0, 0.0, 2.0, 2.0)];
        let ocr = ocr_document();

        let rotated = history
            .push(
                &base,
                PreviewTransformOp::RotateCw,
                first.clone(),
                Some(ocr.clone()),
            )
            .unwrap();
        let crop = PreviewCrop {
            x: 1,
            y: 2,
            width: MIN_CROP_EDGE,
            height: MIN_CROP_EDGE,
        };
        let cropped = history
            .push(
                &rotated.frame,
                PreviewTransformOp::Crop(crop),
                rotated.annotations.clone(),
                rotated.ocr.clone(),
            )
            .unwrap();
        assert_eq!(
            (cropped.frame.width, cropped.frame.height),
            (MIN_CROP_EDGE, MIN_CROP_EDGE)
        );
        assert_eq!(
            cropped.frame.rgba,
            crop_rgba(&rotated.frame, 1, 2, MIN_CROP_EDGE, MIN_CROP_EDGE)
                .unwrap()
                .rgba
        );
        assert!(cropped.can_undo && !cropped.can_redo);

        // 撤销裁剪回到旋转后的状态,再撤销回到基准;重做原样重放。
        let undo_crop = history.undo().unwrap();
        assert_eq!(undo_crop.frame.rgba, rotated.frame.rgba);
        assert_eq!(undo_crop.annotations, rotated.annotations);
        assert_eq!(undo_crop.ocr, rotated.ocr);
        assert!(undo_crop.can_undo && undo_crop.can_redo);

        let undo_rotate = history.undo().unwrap();
        assert_eq!(undo_rotate.frame.rgba, base.rgba);
        assert_eq!(undo_rotate.annotations, first);
        assert_eq!(undo_rotate.ocr, Some(ocr.clone()));
        assert!(!undo_rotate.can_undo && undo_rotate.can_redo);

        let redo_rotate = history.redo().unwrap();
        assert_eq!(redo_rotate.frame.rgba, rotated.frame.rgba);
        assert_eq!(redo_rotate.ocr, rotated.ocr);
        let redo_crop = history.redo().unwrap();
        assert_eq!(redo_crop.frame.rgba, cropped.frame.rgba);
        assert_eq!(redo_crop.annotations, cropped.annotations);
        assert!(history.redo().is_err());
        assert!(history.undo().is_ok());
        assert!(history.undo().is_ok());
        assert!(history.undo().is_err());
    }

    #[test]
    fn preview_transforms_new_push_after_undo_drops_redo_branch() {
        let base = pattern_frame(10, 10);
        let mut history = PreviewTransforms::default();
        let rotated = history
            .push(&base, PreviewTransformOp::RotateCw, Vec::new(), None)
            .unwrap();
        let crop = PreviewCrop {
            x: 0,
            y: 0,
            width: MIN_CROP_EDGE,
            height: MIN_CROP_EDGE,
        };
        history
            .push(
                &rotated.frame,
                PreviewTransformOp::Crop(crop),
                Vec::new(),
                None,
            )
            .unwrap();
        assert!(!history.can_redo());
        history.undo().unwrap();
        assert!(history.can_redo());
        // 撤销分支上的新变换丢弃旧 redo,形成新的前进路径。
        let flipped = history
            .push(
                &rotated.frame,
                PreviewTransformOp::RotateCcw,
                Vec::new(),
                None,
            )
            .unwrap();
        assert!(!flipped.can_redo);
        assert_eq!(flipped.frame.rgba, base.rgba);
        assert!(history.redo().is_err());
    }

    #[test]
    fn preview_transforms_keep_user_edits_between_steps_and_reject_bad_crops() {
        let base = pattern_frame(8, 8);
        let mut history = PreviewTransforms::default();
        let a = vec![rect_annotation(0.0, 0.0, 2.0, 2.0)];
        let first = history
            .push(&base, PreviewTransformOp::RotateCw, a.clone(), None)
            .unwrap();
        // 用户在上一次变换之后新增标注,下一次变换前后的快照都要保留它。
        let mut b = first.annotations.clone();
        b.push(rect_annotation(3.0, 3.0, 2.0, 2.0));
        history
            .push(&first.frame, PreviewTransformOp::RotateCw, b.clone(), None)
            .unwrap();
        let back = history.undo().unwrap();
        assert_eq!(back.annotations, b);
        let back_to_base = history.undo().unwrap();
        assert_eq!(back_to_base.annotations, a);

        // 过小/越界裁剪被拒绝且不写入历史。
        let mut rejecting = PreviewTransforms::default();
        let too_small = rejecting.push(
            &base,
            PreviewTransformOp::Crop(PreviewCrop {
                x: 0,
                y: 0,
                width: MIN_CROP_EDGE - 1,
                height: MIN_CROP_EDGE,
            }),
            Vec::new(),
            None,
        );
        assert!(too_small.is_err());
        assert!(too_small.unwrap_err().message.contains("最小"));
        let out_of_bounds = rejecting.push(
            &base,
            PreviewTransformOp::Crop(PreviewCrop {
                x: 4,
                y: 0,
                width: MIN_CROP_EDGE,
                height: MIN_CROP_EDGE,
            }),
            Vec::new(),
            None,
        );
        assert!(out_of_bounds.is_err());
        assert!(out_of_bounds.unwrap_err().message.contains("超出"));
        assert!(!rejecting.can_undo() && !rejecting.can_redo());

        // 恰好最小边长且贴边的裁剪是合法变换。
        let accepted = rejecting.push(
            &base,
            PreviewTransformOp::Crop(PreviewCrop {
                x: base.width - MIN_CROP_EDGE,
                y: base.height - MIN_CROP_EDGE,
                width: MIN_CROP_EDGE,
                height: MIN_CROP_EDGE,
            }),
            Vec::new(),
            None,
        );
        assert!(accepted.is_ok());
        assert_eq!(
            (
                accepted.as_ref().unwrap().frame.width,
                accepted.as_ref().unwrap().frame.height,
            ),
            (MIN_CROP_EDGE, MIN_CROP_EDGE)
        );
    }
}
