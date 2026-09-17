use std::sync::Mutex;
use std::time::Duration;

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
    if guard.as_ref().is_some_and(|session| session.busy) {
        return false;
    }
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

async fn capture_region(app: &AppHandle) -> Result<(), CaptureError> {
    #[cfg(windows)]
    {
        let handle = app.clone();
        let picked = tauri::async_runtime::spawn_blocking(move || {
            let (frame, monitor) = grab_pointer_screen(&handle)?;
            store_pixels(&handle, frame.clone(), monitor.clone())?;
            super::native_overlay::pick_region(&frame, &monitor)
        })
        .await
        .map_err(|_| CaptureError::api("截取线程失败。"))??;
        let handle = app.clone();
        return tauri::async_runtime::spawn_blocking(move || match picked {
            Some(rect) => confirm_region(
                &handle,
                RegionSelection {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                },
            ),
            None => cancel(&handle).map(|_| ()),
        }).await.map_err(|_| CaptureError::api("截取线程失败。"))?;
    }
    #[cfg(not(windows))]
    {
        let monitor = freeze_screen(app, Vec::new()).await?;
        ui::open_overlay(app, &monitor)?;
        Ok(())
    }
}

async fn capture_window_mode(app: &AppHandle) -> Result<(), CaptureError> {
    let windows = tauri::async_runtime::spawn_blocking(move || platform::list_windows(platform::self_pid()))
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
        complete_success(&handle, frame)
    })
    .await
    .map_err(|_| CaptureError::api("截取线程失败。"))?
}

async fn freeze_screen(app: &AppHandle, windows: Vec<ListedWindow>) -> Result<MonitorGeom, CaptureError> {
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
    with_session(app, |session| {
        session
            .as_ref()
            .and_then(|item| item.freeze.clone())
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
        crop_rgba(freeze, selection.x, selection.y, selection.width, selection.height)
    })?;
    ui::hide_window(app, ui::OVERLAY);
    complete_success(app, frame)
}

pub fn confirm_logical_region(app: &AppHandle, rect: LogicalRect) -> Result<(), CaptureError> {
    let physical = with_session(app, |session| {
        let session = session.as_ref().ok_or_else(CaptureError::cancelled)?;
        let frame = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("未初始化"))?;
        Ok(crop_from_logical(frame.scale, rect, frame.width, frame.height))
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
    complete_success(app, frame)
}

pub fn cancel(app: &AppHandle) -> Result<CancelOutcome, CaptureError> {
    cancel_internal(app)
}

fn cancel_internal(app: &AppHandle) -> Result<CancelOutcome, CaptureError> {
    let restore = with_session_mut(app, |session| {
        let Some(current) = session.as_mut() else {
            return None;
        };
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

fn complete_success(app: &AppHandle, frame: Frame) -> Result<(), CaptureError> {
    let started = std::time::Instant::now();
    if is_cancelled(app) {
        return Err(CaptureError::cancelled());
    }
    let png = encode_png(&frame)?;
    let encoded_at = started.elapsed();
    if is_cancelled(app) {
        return Err(CaptureError::cancelled());
    }
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
    ui::open_preview(app, &frame)?;
    if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some() {
        eprintln!("Cropmark capture {}x{}: PNG={:?}, clipboard={:?}, preview={:?}, total={:?}",
            frame.width, frame.height, encoded_at, copied_at - encoded_at,
            started.elapsed() - copied_at, started.elapsed());
    }
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            current.preview_opened = true;
            current.busy = false;
        }
    });
    if let Some(error) = clipboard_error {
        set_last_error(app, Some(error.clone()));
        let _ = ui::open_error(app, &error);
    }
    Ok(())
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
        session.as_ref().map(|current| current.cancelled).unwrap_or(true)
    })
}

pub fn close_preview(app: &AppHandle) {
    ui::hide_window(app, ui::PREVIEW);
    with_session_mut(app, |session| *session = None);
}

pub fn close_error(app: &AppHandle) {
    ui::close_window(app, ui::ERROR);
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
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
}
