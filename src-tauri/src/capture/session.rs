use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager};

use super::buffer::{crop_rgba, encode_png, Frame};
use super::error::CaptureError;
use super::geometry::{crop_from_logical, monitor_at_physical, LogicalRect, MonitorGeom};
use super::hide::{
    grab_allowed, hide_not_presented_error, plan_delay, wait_compositor_presented,
    wait_until_hidden, HideWait, RecordedSurface, SurfaceKind,
};
use super::platform;
use super::ui::{self, DelayPayload, OverlayPayload, PreviewPayload};
use super::windows_list::ListedWindow;
use crate::clipboard::{self, ClipboardGuard};
use crate::hotkeys::CaptureMode;

pub struct CaptureRuntime {
    inner: Mutex<Option<ActiveSession>>,
    last_error: Mutex<Option<CaptureError>>,
}

impl Default for CaptureRuntime {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
            last_error: Mutex::new(None),
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
    /// 贴图再标注(R9):预览会话的回写目标 label;普通截取预览为 None,
    /// 会话被替换/关闭时随之失效。
    writeback: Option<String>,
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
            writeback: None,
        }
    }
}

/// 看门狗:正常截取远小于 30s;busy 超时说明壳消息泵/线程卡死,
/// 强制重置旧会话,避免"取消一次后再也无法截取"的静默死锁。
const STALE_SESSION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeginDecision {
    IgnoreBusy,
    ResetStaleThenBegin,
    Begin,
}

fn begin_decision(session: Option<&ActiveSession>, now: Instant) -> BeginDecision {
    match session {
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
fn occupy_session(
    slot: &mut Option<ActiveSession>,
    mode: CaptureMode,
    delay_ms: u64,
    now: Instant,
) -> bool {
    match begin_decision(slot.as_ref(), now) {
        BeginDecision::IgnoreBusy => false,
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
/// opening the preview window (R3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuietAction {
    Copy,
    Save,
    Pin,
    Ocr,
}

/// Retention window for the frame kept after a quiet finish.
pub const DEFAULT_FRAME_TTL: Duration = Duration::from_secs(30);

pub fn begin(app: &AppHandle, mode: CaptureMode, delay_ms: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run(app.clone(), mode, delay_ms).await {
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

async fn run(app: AppHandle, mode: CaptureMode, delay_ms: u64) -> Result<(), CaptureError> {
    if !try_begin_with_delay(&app, mode, delay_ms) {
        return Ok(());
    }
    hide_product_surfaces(&app)?;
    wait_delay_before_capture(&app, delay_ms, mode).await?;
    match mode {
        CaptureMode::Region => capture_region(&app).await,
        CaptureMode::Window => capture_window_mode(&app).await,
        CaptureMode::Fullscreen => capture_fullscreen(&app).await,
    }
}

/// 隐藏前置完成后按延时计划展示并等待倒计时(可取消);0 秒立即返回。
async fn wait_delay_before_capture(
    app: &AppHandle,
    delay_ms: u64,
    mode: CaptureMode,
) -> Result<(), CaptureError> {
    let plan = plan_delay(delay_ms);
    if plan.delay_ms > 0 && !plan.overlay_during_delay {
        show_delay(app, plan.delay_ms, mode)?;
        if wait_delay(app, plan.delay_ms).await? {
            hide_session_surface(app, ui::DELAY)?;
            hide_product_surfaces(app)?;
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
    if !try_begin_with_delay(&app, CaptureMode::Region, delay_ms) {
        return Ok(());
    }
    hide_product_surfaces(&app)?;
    wait_delay_before_capture(&app, delay_ms, CaptureMode::Region).await?;
    capture_last_region(&app, plan).await
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

/// 抓取目标显示器并按钳制后的区域裁剪;与全屏/区域完成一样走
/// `finish_configured`,预览/静默完成、自动复制、历史与 toast 行为一致。
async fn capture_last_region(app: &AppHandle, plan: FixedRegionPlan) -> Result<(), CaptureError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_capture_ready(&handle)?;
        let frame = platform::capture_monitor(&plan.monitor)?;
        let (x, y, width, height) = local_crop(&plan.monitor, &plan.region)
            .ok_or_else(|| CaptureError::api("error.capture.last_region_out_of_range"))?;
        let cropped = crop_rgba(&frame, x, y, width, height)?;
        finish_configured(&handle, cropped).map(|_| ())
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

fn try_begin_with_delay(app: &AppHandle, mode: CaptureMode, delay_ms: u64) -> bool {
    let runtime = app.state::<CaptureRuntime>();
    let mut guard = lock(&runtime.inner);
    let now = Instant::now();
    if begin_decision(guard.as_ref(), now) == BeginDecision::ResetStaleThenBegin {
        eprintln!("Cropmark: stale busy session reset after {STALE_SESSION_TIMEOUT:?}");
        for label in ui::session_window_labels() {
            ui::hide_window(app, label);
        }
    }
    if !occupy_session(&mut guard, mode, delay_ms, now) {
        return false;
    }
    // A lingering toast must not leak into the next capture (hide-before-capture);
    // 仅隐藏——toast 窗是预创建复用的 webview,关闭会破坏复用。
    ui::hide_window(app, ui::TOAST);
    *lock(&runtime.last_error) = None;
    true
}

fn hide_product_surfaces(app: &AppHandle) -> Result<(), CaptureError> {
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
    for label in ui::session_window_labels() {
        if label != ui::DELAY && ui::is_visible(app, label) {
            ui::hide_window(app, label);
        }
    }
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
        if session.hide.recorded.is_empty() {
            let mut hide = HideWait::record(recorded);
            hide.request_hide();
            hide.commit_presented(true, true)?;
            session.hide = hide;
        } else {
            session.hide.commit_presented(true, true)?;
        }
        Ok(())
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

async fn wait_delay(app: &AppHandle, delay_ms: u64) -> Result<bool, CaptureError> {
    let steps = (delay_ms / 100).max(1);
    for _ in 0..steps {
        if is_cancelled(app) {
            cancel_internal(app)?;
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
    if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some() {
        eprintln!("Cropmark color copy: hook ran, hex={hex}");
    }
    let toast_key = |key: &str, params: &[(&str, &str)]| {
        SHELL_APP.with(|slot| {
            if let Some(app) = slot.borrow().as_ref() {
                ui::show_toast_key_params(app, key, params);
            } else if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some() {
                eprintln!("Cropmark color copy: no app in thread-local");
            }
        });
    };
    match clipboard::copy_text(text) {
        Ok(()) => toast_key("toast.color_copied", &[("hex", hex)]),
        Err(_) => toast_key("toast.color_copy_failed", &[]),
    }
}

/// 把设置里的功能入口开关映射为选区引擎 FeatureFlags(字段一一对应)。
/// 每次截取启动时读取,关闭的入口下一次截取即消失。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn feature_flags_from(features: crate::settings::FeatureSettings) -> super::selection::FeatureFlags {
    super::selection::FeatureFlags {
        ocr_entry: features.ocr_entry,
        pin_entry: features.pin_entry,
        magnifier: features.magnifier,
        toolbar_copy: features.toolbar_copy,
        toolbar_save: features.toolbar_save,
        toolbar_pin: features.toolbar_pin,
    }
}

/// 原生壳区域路径(Windows/macOS/Linux X11):冻结指针所在屏像素并交给
/// 平台壳,按壳结果走 Preview/Quiet/取消分发(三平台同构,ADR-008)。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
async fn capture_region_native(app: &AppHandle) -> Result<(), CaptureError> {
    use super::native_overlay::RegionOutcome;

    let handle = app.clone();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let (frame, monitor) = grab_pointer_screen(&handle)?;
        store_pixels(&handle, frame.clone(), monitor.clone())?;
        let flags = feature_flags_from(crate::settings::current_features(&handle));
        // 壳回调在同一线程内同步执行,经 thread-local 取回 AppHandle。
        SHELL_APP.with(|slot| *slot.borrow_mut() = Some(handle.clone()));
        let picked = super::native_overlay::pick_region(
            &frame,
            &monitor,
            flags,
            super::native_overlay::ShellHooks {
                copy_color: copy_color_feedback,
            },
        );
        SHELL_APP.with(|slot| *slot.borrow_mut() = None);
        picked
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))??;
    match picked {
        // Enter 确认:按 finishAction 选择预览或静默(复制+toast)。
        RegionOutcome::Preview(rect) => {
            let handle = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                confirm_region(
                    &handle,
                    RegionSelection {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                    },
                )
            })
            .await
            .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
        }
        // 「标注」动作:强制打开预览编辑器,静默完成配置不适用于显式标注(R4 review)。
        RegionOutcome::Annotate(rect) => {
            let handle = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                annotate_region(
                    &handle,
                    RegionSelection {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: rect.height,
                    },
                )
            })
            .await
            .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
        }
        // 操作条/菜单动作:静默完成并执行动作(不开预览)。
        RegionOutcome::Quiet(rect, action) => {
            finish_region_with(
                app,
                RegionSelection {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
                action,
            )
            .await
        }
        RegionOutcome::Cancelled => {
            let handle = app.clone();
            tauri::async_runtime::spawn_blocking(move || cancel(&handle).map(|_| ()))
                .await
                .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
async fn capture_region(app: &AppHandle) -> Result<(), CaptureError> {
    capture_region_native(app).await
}

/// Linux 区域路径按会话类型分派:判定复用 `platform::linux_capture_backend`
/// (WAYLAND_DISPLAY 非空 → portal/Wayland),并额外要求 `$DISPLAY` 可用
/// (含 XWayland);其余情况与既有行为一致走 Web 覆盖层(portal 抓屏不变)。
#[cfg(target_os = "linux")]
async fn capture_region(app: &AppHandle) -> Result<(), CaptureError> {
    if linux_uses_native_selection() {
        return capture_region_native(app).await;
    }
    let monitor = freeze_screen(app, Vec::new()).await?;
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

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
async fn capture_region(app: &AppHandle) -> Result<(), CaptureError> {
    let monitor = freeze_screen(app, Vec::new()).await?;
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

async fn capture_window_mode(app: &AppHandle) -> Result<(), CaptureError> {
    let windows =
        tauri::async_runtime::spawn_blocking(move || platform::list_windows(platform::self_pid()))
            .await
            .map_err(|_| CaptureError::api("error.capture.window_list"))?;
    let windows = windows_for_window_mode(windows)?;
    let monitor = freeze_screen(app, windows).await?;
    ui::open_overlay(app, &monitor)?;
    Ok(())
}

async fn capture_fullscreen(app: &AppHandle) -> Result<(), CaptureError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (frame, _) = grab_pointer_screen(&handle)?;
        finish_configured(&handle, frame).map(|_| ())
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

async fn freeze_screen(
    app: &AppHandle,
    windows: Vec<ListedWindow>,
) -> Result<MonitorGeom, CaptureError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (frame, monitor) = grab_pointer_screen(&handle)?;
        store_freeze(&handle, frame, monitor.clone(), windows)?;
        Ok(monitor)
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

fn grab_pointer_screen(app: &AppHandle) -> Result<(Frame, MonitorGeom), CaptureError> {
    require_capture_ready(app)?;
    let monitor = tauri_pointer_monitor(app).unwrap_or(platform::pointer_monitor()?);
    let frame = platform::capture_monitor(&monitor)?;
    Ok((frame, monitor))
}

fn require_capture_ready(app: &AppHandle) -> Result<(), CaptureError> {
    with_session(app, |session| {
        let wait = session
            .as_ref()
            .map(|current| &current.hide)
            .ok_or_else(hide_not_presented_error)?;
        grab_allowed(wait)
    })
}

/// 当前所有显示器(物理几何与缩放),供指针命中与"上次区域"钳制共用。
fn tauri_monitors(app: &AppHandle) -> Vec<MonitorGeom> {
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

fn store_pixels(app: &AppHandle, frame: Frame, monitor: MonitorGeom) -> Result<(), CaptureError> {
    with_session_mut(app, |session| {
        let session = session.as_mut().ok_or_else(CaptureError::cancelled)?;
        if session.cancelled {
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
) -> Result<(), CaptureError> {
    let mode = with_session(app, |session| {
        session
            .as_ref()
            .map(|current| current.mode)
            .ok_or_else(CaptureError::cancelled)
    })?;
    let overlay = ui::overlay_payload(
        mode,
        &frame,
        &monitor,
        windows.clone(),
        overlay_reduced_capabilities(mode, region_native_shell()),
    )?;
    with_session_mut(app, |session| {
        let session = session.as_mut().ok_or_else(CaptureError::cancelled)?;
        if session.cancelled {
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
    let busy = with_session(app, |session| {
        session.as_ref().is_some_and(|current| current.busy)
    });
    if busy {
        return Err(CaptureError::api("error.capture.pin_busy"));
    }
    let png = encode_png(&frame)?;
    let preview = ui::preview_payload(&frame, &png, ui::PreviewCopyState::Disabled);
    with_session_mut(app, |session| {
        let mut current = ActiveSession::new(CaptureMode::Region, 0, Instant::now());
        current.busy = false;
        current.preview_opened = true;
        current.freeze = Some(frame.clone());
        current.preview = Some(preview);
        current.writeback = Some(writeback);
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

pub fn confirm_region(app: &AppHandle, selection: RegionSelection) -> Result<(), CaptureError> {
    finish_selection(app, selection, FinishIntent::Configured).map(|_| ())
}

/// 显式「标注」请求(原生选区壳的「标注」动作/右键菜单):总是打开预览编辑器,
/// 不受静默完成设置影响;裁剪帧与普通完成一样按 autoCopy 决定是否写剪贴板。
fn annotate_region(app: &AppHandle, selection: RegionSelection) -> Result<(), CaptureError> {
    finish_selection(app, selection, FinishIntent::Annotate).map(|_| ())
}

fn finish_selection(
    app: &AppHandle,
    selection: RegionSelection,
    intent: FinishIntent,
) -> Result<FinishSummary, CaptureError> {
    let frame = with_session(app, |session| {
        let session = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        let freeze = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("error.capture.buffer_uninitialized"))?;
        crop_rgba(
            freeze,
            selection.x,
            selection.y,
            selection.width,
            selection.height,
        )
    })?;
    ui::hide_window(app, ui::OVERLAY);
    let summary = finish_frame(app, frame, intent)?;
    // R6:成功完成的区域截图覆盖"上次区域",托盘直取从下一次打开菜单起可用。
    remember_selection_region(app, &selection);
    Ok(summary)
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
    )
}

pub fn confirm_window(app: &AppHandle, window_id: String) -> Result<(), CaptureError> {
    ui::hide_window(app, ui::OVERLAY);
    require_capture_ready(app)?;
    if is_cancelled(app) {
        return Err(CaptureError::cancelled());
    }
    let frame = platform::capture_window(&window_id)?;
    finish_configured(app, frame).map(|_| ())
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

/// 带守卫的静默完成入口:命令层 `finish_region_with` 与 Windows 选区壳的
/// 操作条/菜单动作共用。完成后按动作给出反馈;复制在剪贴板写入失败时
/// 提示失败而非「已复制」。
pub async fn finish_region_with(
    app: &AppHandle,
    selection: RegionSelection,
    action: QuietAction,
) -> Result<(), CaptureError> {
    let handle = app.clone();
    let finish = tauri::async_runtime::spawn_blocking(move || {
        if !quiet_finish_allowed(&handle) {
            return Err(CaptureError::api("error.capture.region_missing"));
        }
        finish_region_quiet(&handle, selection, DEFAULT_FRAME_TTL)
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
    ttl: Duration,
) -> Result<FinishSummary, CaptureError> {
    let frame = with_session(app, |session| {
        let session = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        let freeze = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("error.capture.buffer_uninitialized"))?;
        crop_rgba(
            freeze,
            selection.x,
            selection.y,
            selection.width,
            selection.height,
        )
    })?;
    ui::hide_window(app, ui::OVERLAY);
    // 显式动作(操作条/菜单复制等)不受 autoCopy 开关影响,始终写剪贴板。
    let summary = finish_with_ttl(app, frame, FinishDisposition::Quiet, ttl, true)?;
    // R6:静默完成同属成功完成的区域截图,同样刷新"上次区域"。
    remember_selection_region(app, &selection);
    Ok(summary)
}

pub fn cancel(app: &AppHandle) -> Result<CancelOutcome, CaptureError> {
    cancel_internal(app)
}

/// Drops the session immediately. Cancel never writes the clipboard, a file,
/// or a preview window; product surfaces recorded before hide are restored.
fn take_cancel_plan(
    session: &mut Option<ActiveSession>,
) -> Option<(CancelOutcome, Vec<RecordedSurface>)> {
    let current = session.as_mut()?;
    current.cancelled = true;
    let restore = current.hide.restore_on_cancel();
    *session = None;
    Some((CancelOutcome::clean(), restore))
}

fn cancel_internal(app: &AppHandle) -> Result<CancelOutcome, CaptureError> {
    let planned = with_session_mut(app, take_cancel_plan);
    let Some((outcome, restore)) = planned else {
        return Ok(CancelOutcome::clean());
    };
    for label in ui::session_window_labels() {
        ui::hide_window(app, label);
    }
    for surface in restore {
        ui::show_window(app, &surface.label);
    }
    let _ = app.emit("capture-cancelled", ());
    Ok(outcome)
}

/// 普通捕获(区域确认/窗口/全屏)的统一完成入口:按当前设置选择预览或
/// 静默(复制后关闭),静默完成仍给出 toast 反馈,避免用户感知为无响应(R4)。
fn finish_configured(app: &AppHandle, frame: Frame) -> Result<FinishSummary, CaptureError> {
    finish_frame(app, frame, FinishIntent::Configured)
}

fn finish_frame(
    app: &AppHandle,
    frame: Frame,
    intent: FinishIntent,
) -> Result<FinishSummary, CaptureError> {
    let capture = crate::settings::current_capture(app);
    let disposition = finish_disposition(intent, capture);
    let summary = finish_with_ttl(
        app,
        frame,
        disposition,
        DEFAULT_FRAME_TTL,
        capture.auto_copy,
    )?;
    if disposition == FinishDisposition::Quiet {
        ui::show_toast_key(
            app,
            if summary.clipboard_written {
                "toast.copied"
            } else {
                "toast.copy_failed"
            },
        );
    }
    Ok(summary)
}

/// 完成请求来源:普通完成(Enter/确认/窗口/全屏)套用 finishAction;显式
/// 「标注」总是进预览编辑器——静默配置只约束默认完成动作,不能吞掉标注意图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinishIntent {
    Configured,
    Annotate,
}

fn finish_disposition(
    intent: FinishIntent,
    capture: crate::settings::CaptureSettings,
) -> FinishDisposition {
    match intent {
        FinishIntent::Annotate => FinishDisposition::Preview,
        FinishIntent::Configured => configured_disposition(capture),
    }
}

/// 静默完成必须伴随自动复制(autoCopy 关闭时 sanitize 已强制回退预览,
/// 这里对内存值再做一次兜底),避免"静默且无输出"的空动作。
fn configured_disposition(capture: crate::settings::CaptureSettings) -> FinishDisposition {
    if capture.auto_copy && capture.finish_action == crate::settings::FinishAction::Quiet {
        FinishDisposition::Quiet
    } else {
        FinishDisposition::Preview
    }
}

fn finish_with_ttl(
    app: &AppHandle,
    frame: Frame,
    disposition: FinishDisposition,
    frame_ttl: Duration,
    auto_copy: bool,
) -> Result<FinishSummary, CaptureError> {
    let started = Instant::now();
    if is_cancelled(app) {
        return Err(CaptureError::cancelled());
    }
    let png = encode_png(&frame)?;
    let encoded_at = started.elapsed();
    if is_cancelled(app) {
        return Err(CaptureError::cancelled());
    }
    // autoCopy 关闭时不写剪贴板;显式复制路径不受此开关影响。
    let clipboard_error = if auto_copy {
        clipboard::copy_frame_with_png(&frame, &png).err()
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
    let preview = ui::preview_payload(&frame, &png, copy_state);
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            if clipboard_written {
                current.clipboard.commit_success();
            }
            current.freeze = Some(frame.clone());
            current.preview = Some(preview);
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
    if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some() {
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
    // 全部在 spawn_blocking 内,不阻塞完成路径。
    crate::history::record_capture(app, frame);
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
    ui::open_error(app, &error)?;
    Ok(())
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
}

pub fn close_error(app: &AppHandle) {
    // 仅隐藏:error 窗是预创建复用的 webview。
    ui::hide_window(app, ui::ERROR);
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

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    #[test]
    fn feature_flags_mirror_stored_feature_settings() {
        let all_on = feature_flags_from(crate::settings::FeatureSettings::default());
        assert_eq!(all_on, super::super::selection::FeatureFlags::default());
        let all_off = feature_flags_from(crate::settings::FeatureSettings {
            ocr_entry: false,
            pin_entry: false,
            magnifier: false,
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
        });
        assert!(!all_off.ocr_entry);
        assert!(!all_off.pin_entry);
        assert!(!all_off.magnifier);
        assert!(!all_off.toolbar_copy);
        assert!(!all_off.toolbar_save);
        assert!(!all_off.toolbar_pin);
    }

    fn hide_before_capture_steps(mode: CaptureMode, delay_ms: u64) -> Vec<SessionStep> {
        session_steps(!matches!(mode, CaptureMode::Fullscreen), delay_ms)
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
                        assert_eq!(steps.last().copied(), Some(SessionStep::OpenPreview));
                        assert!(!steps.contains(&SessionStep::ShowOverlayOnFreeze));
                    }
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

    #[test]
    fn fullscreen_skips_overlay() {
        let steps = session_steps(false, 0);
        assert_eq!(steps.last().copied(), Some(SessionStep::OpenPreview));
        assert!(!steps.contains(&SessionStep::ShowOverlayOnFreeze));
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
    fn cancel_clears_session_without_clipboard_file_or_preview() {
        let mut session = ActiveSession::new(CaptureMode::Region, 0, Instant::now());
        session.clipboard.commit_success();
        session.file_written = true;
        session.preview_opened = true;
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
        let mut slot = Some(session);
        let (outcome, restore) = take_cancel_plan(&mut slot).expect("had session");
        assert!(slot.is_none());
        assert!(!outcome.clipboard_written);
        assert!(!outcome.file_written);
        assert!(!outcome.preview_opened);
        let labels: Vec<_> = restore
            .iter()
            .map(|surface| surface.label.as_str())
            .collect();
        assert_eq!(labels, ["preview", "settings"]);
        assert!(take_cancel_plan(&mut slot).is_none());
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
    fn configured_disposition_requires_auto_copy_for_quiet() {
        use crate::settings::{CaptureSettings, FinishAction};

        let quiet = CaptureSettings {
            delay_seconds: 0,
            auto_copy: true,
            finish_action: FinishAction::Quiet,
        };
        assert_eq!(configured_disposition(quiet), FinishDisposition::Quiet);
        assert_eq!(
            configured_disposition(CaptureSettings::default()),
            FinishDisposition::Preview
        );
        // autoCopy 关闭时即使内存值仍为 quiet 也回退预览。
        let off = CaptureSettings {
            auto_copy: false,
            ..quiet
        };
        assert_eq!(configured_disposition(off), FinishDisposition::Preview);
    }

    #[test]
    fn annotate_intent_forces_preview_while_enter_keeps_quiet() {
        use crate::settings::{CaptureSettings, FinishAction};

        let quiet = CaptureSettings {
            delay_seconds: 0,
            auto_copy: true,
            finish_action: FinishAction::Quiet,
        };
        // Enter/确认仍按设置静默完成。
        assert_eq!(
            finish_disposition(FinishIntent::Configured, quiet),
            FinishDisposition::Quiet
        );
        // 显式「标注」不受静默配置影响,总是打开预览编辑器。
        assert_eq!(
            finish_disposition(FinishIntent::Annotate, quiet),
            FinishDisposition::Preview
        );
        // 默认设置与 autoCopy 关闭的回退两边都是预览。
        assert_eq!(
            finish_disposition(FinishIntent::Annotate, CaptureSettings::default()),
            FinishDisposition::Preview
        );
        let off = CaptureSettings {
            auto_copy: false,
            ..quiet
        };
        assert_eq!(
            finish_disposition(FinishIntent::Configured, off),
            FinishDisposition::Preview
        );
        assert_eq!(
            finish_disposition(FinishIntent::Annotate, off),
            FinishDisposition::Preview
        );
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
}
