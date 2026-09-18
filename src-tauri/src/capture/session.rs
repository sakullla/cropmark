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

async fn run(app: AppHandle, mode: CaptureMode, delay_ms: u64) -> Result<(), CaptureError> {
    if !try_begin_with_delay(&app, mode, delay_ms) {
        return Ok(());
    }
    hide_product_surfaces(&app)?;
    let plan = plan_delay(delay_ms);
    if plan.delay_ms > 0 && !plan.overlay_during_delay {
        show_delay(&app, plan.delay_ms, mode)?;
        if wait_delay(&app, plan.delay_ms).await? {
            hide_session_surface(&app, ui::DELAY)?;
            hide_product_surfaces(&app)?;
        }
    }
    match mode {
        CaptureMode::Region => capture_region(&app).await,
        CaptureMode::Window => capture_window_mode(&app).await,
        CaptureMode::Fullscreen => capture_fullscreen(&app).await,
    }
}

fn try_begin_with_delay(app: &AppHandle, mode: CaptureMode, delay_ms: u64) -> bool {
    let runtime = app.state::<CaptureRuntime>();
    let mut guard = lock(&runtime.inner);
    // 看门狗:正常截取远小于 30s;busy 超时说明壳消息泵/线程卡死,
    // 强制重置旧会话,避免"取消一次后再也无法截取"的静默死锁。
    const STALE_SESSION_TIMEOUT: Duration = Duration::from_secs(30);
    if guard.as_ref().is_some_and(|session| {
        session.busy && session.started_at.elapsed() > STALE_SESSION_TIMEOUT
    }) {
        eprintln!("Cropmark: stale busy session reset after {STALE_SESSION_TIMEOUT:?}");
        for label in ui::session_window_labels() {
            ui::hide_window(app, label);
        }
        *guard = None;
    }
    if guard.as_ref().is_some_and(|session| session.busy) {
        return false;
    }
    // A lingering toast must not leak into the next capture (hide-before-capture);
    // 仅隐藏——toast 窗是预创建复用的 webview,关闭会破坏复用。
    ui::hide_window(app, ui::TOAST);
    // Replacing the session drops any frame retained by a previous quiet finish.
    *guard = Some(ActiveSession {
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
        started_at: Instant::now(),
    });
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
    let toast = |message: String| {
        SHELL_APP.with(|slot| {
            if let Some(app) = slot.borrow().as_ref() {
                ui::show_toast(app, &message);
            } else if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some() {
                eprintln!("Cropmark color copy: no app in thread-local");
            }
        });
    };
    match clipboard::copy_text(text) {
        Ok(()) => toast(format!("已复制色值 {hex}。")),
        Err(_) => toast("复制色值失败，请重试。".to_string()),
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
    .map_err(|_| CaptureError::api("截取线程失败。"))??;
    match picked {
        // Enter/标注:沿用 Preview 完成路径(裁剪+剪贴板+预览)。
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
            .map_err(|_| CaptureError::api("截取线程失败。"))?
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
                .map_err(|_| CaptureError::api("截取线程失败。"))?
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

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
async fn capture_region(app: &AppHandle) -> Result<(), CaptureError> {
    let monitor = freeze_screen(app, Vec::new()).await?;
    ui::open_overlay(app, &monitor)?;
    Ok(())
}

async fn capture_window_mode(app: &AppHandle) -> Result<(), CaptureError> {
    let windows =
        tauri::async_runtime::spawn_blocking(move || platform::list_windows(platform::self_pid()))
            .await
            .map_err(|_| CaptureError::api("无法列出窗口。"))??;
    if windows.is_empty() {
        return Err(CaptureError::unavailable(
            "没有可截取的窗口，或当前桌面无法列出窗口。请改用区域或全屏截取。",
        ));
    }
    let monitor = freeze_screen(app, windows).await?;
    ui::open_overlay(app, &monitor)?;
    Ok(())
}

async fn capture_fullscreen(app: &AppHandle) -> Result<(), CaptureError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (frame, _) = grab_pointer_screen(&handle)?;
        finish(&handle, frame, FinishDisposition::Preview).map(|_| ())
    })
    .await
    .map_err(|_| CaptureError::api("截取线程失败。"))?
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
    .map_err(|_| CaptureError::api("截取线程失败。"))?
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

fn tauri_pointer_monitor(app: &AppHandle) -> Option<MonitorGeom> {
    let position = app.cursor_position().ok()?;
    let monitors = app.available_monitors().ok()?;
    let geoms: Vec<MonitorGeom> = monitors
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
        .collect();
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
    let overlay = ui::overlay_payload(mode, &frame, &monitor, windows.clone())?;
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
            .ok_or_else(|| CaptureError::api("没有正在进行的截取。"))
    })
}

pub fn preview_frame(app: &AppHandle) -> Result<PreviewPayload, CaptureError> {
    with_session(app, |session| {
        session
            .as_ref()
            .and_then(|item| item.preview.clone())
            .ok_or_else(|| CaptureError::api("没有可预览的截图。"))
    })
}

pub fn current_preview_frame(app: &AppHandle) -> Result<Frame, CaptureError> {
    let now = Instant::now();
    with_session_mut(app, |session| {
        let Some(current) = session.as_mut() else {
            return Err(CaptureError::api("没有可预览的截图。"));
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
            .ok_or_else(|| CaptureError::api("没有可预览的截图。"))
    })
}

pub fn mark_preview_file_written(app: &AppHandle) {
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            current.file_written = true;
        }
    });
}

pub fn confirm_region(app: &AppHandle, selection: RegionSelection) -> Result<(), CaptureError> {
    let frame = with_session(app, |session| {
        let session = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        let freeze = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("未初始化"))?;
        crop_rgba(
            freeze,
            selection.x,
            selection.y,
            selection.width,
            selection.height,
        )
    })?;
    ui::hide_window(app, ui::OVERLAY);
    finish(app, frame, FinishDisposition::Preview).map(|_| ())
}

pub fn confirm_logical_region(app: &AppHandle, rect: LogicalRect) -> Result<(), CaptureError> {
    let physical = with_session(app, |session| {
        let session = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        let frame = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("未初始化"))?;
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
    finish(app, frame, FinishDisposition::Preview).map(|_| ())
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
            return Err(CaptureError::api("当前没有进行中的区域截取。"));
        }
        finish_region_quiet(&handle, selection, DEFAULT_FRAME_TTL)
    })
    .await
    .map_err(|_| CaptureError::api("截取线程失败。"))??;
    match action {
        QuietAction::Copy => {
            if finish.clipboard_written {
                ui::show_toast(app, "已复制到剪贴板。");
            } else {
                ui::show_toast(app, "复制失败，请重试。");
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
            .ok_or_else(|| CaptureError::invalid_buffer("未初始化"))?;
        crop_rgba(
            freeze,
            selection.x,
            selection.y,
            selection.width,
            selection.height,
        )
    })?;
    ui::hide_window(app, ui::OVERLAY);
    finish_with_ttl(app, frame, FinishDisposition::Quiet, ttl)
}

pub fn cancel(app: &AppHandle) -> Result<CancelOutcome, CaptureError> {
    cancel_internal(app)
}

fn cancel_internal(app: &AppHandle) -> Result<CancelOutcome, CaptureError> {
    let restore = with_session_mut(app, |session| {
        let current = session.as_mut()?;
        current.cancelled = true;
        Some(current.hide.restore_on_cancel())
    });
    let Some(restore) = restore else {
        return Ok(CancelOutcome::clean());
    };
    for label in ui::session_window_labels() {
        ui::hide_window(app, label);
    }
    for surface in restore {
        ui::show_window(app, &surface.label);
    }
    with_session_mut(app, |session| *session = None);
    let _ = app.emit("capture-cancelled", ());
    Ok(CancelOutcome::clean())
}

fn finish(
    app: &AppHandle,
    frame: Frame,
    disposition: FinishDisposition,
) -> Result<FinishSummary, CaptureError> {
    finish_with_ttl(app, frame, disposition, DEFAULT_FRAME_TTL)
}

fn finish_with_ttl(
    app: &AppHandle,
    frame: Frame,
    disposition: FinishDisposition,
    frame_ttl: Duration,
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
    // Both dispositions keep today's contract: the unannotated PNG enters the clipboard.
    let clipboard_error = clipboard::copy_frame_with_png(&frame, &png).err();
    let copied_at = started.elapsed();
    let preview = ui::preview_payload(&frame, &png, clipboard_error.is_none());
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            if clipboard_error.is_none() {
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
    Ok(FinishSummary {
        clipboard_written: clipboard_error.is_none(),
    })
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
    error
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
pub fn cancel_without_side_effects(
    clipboard_written: bool,
    file_written: bool,
    preview_opened: bool,
) -> CancelOutcome {
    let _ = (clipboard_written, file_written, preview_opened);
    CancelOutcome::clean()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::hide::{
        grab_allowed, session_steps, HideWait, RecordedSurface, SessionStep, SurfaceKind,
    };

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

    #[test]
    fn cancel_has_no_clipboard_file_or_preview() {
        let outcome = cancel_without_side_effects(true, true, true);
        assert!(!outcome.clipboard_written);
        assert!(!outcome.file_written);
        assert!(!outcome.preview_opened);
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
        ActiveSession {
            mode: CaptureMode::Region,
            busy: true,
            delay_ms: 0,
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
            started_at: Instant::now(),
        }
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
}
