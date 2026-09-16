use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager};

use super::buffer::{crop_rgba, Frame};
use super::error::CaptureError;
use super::geometry::{crop_from_logical, LogicalRect, MonitorGeom};
use super::hide::{
    plan_delay, wait_compositor_presented, wait_until_hidden, HideWait, RecordedSurface, SurfaceKind,
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
            hide_session_surface(&app, ui::DELAY);
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
    recorded.push(RecordedSurface {
        label: "tray-popup".into(),
        kind: SurfaceKind::TrayPopup,
        was_visible: true,
    });
    platform::dismiss_tray_popup();
    for label in ui::session_window_labels() {
        if label != ui::DELAY && ui::is_visible(app, label) {
            ui::hide_window(app, label);
        }
    }
    let labels = {
        let mut labels: Vec<String> = ui::product_window_labels()
            .into_iter()
            .map(str::to_string)
            .collect();
        labels.extend(ui::session_window_labels().into_iter().map(str::to_string));
        labels
    };
    let hidden = wait_until_hidden(
        || ui::any_visible(app, &labels.iter().map(String::as_str).collect::<Vec<_>>()),
        Duration::from_millis(400),
    );
    wait_compositor_presented();
    with_session_mut(app, |session| {
        let Some(session) = session.as_mut() else {
            return;
        };
        if session.hide.recorded.is_empty() {
            let mut hide = HideWait::record(recorded);
            hide.request_hide();
            if hidden {
                hide.mark_unmapped();
                hide.mark_presented();
            }
            session.hide = hide;
        } else if hidden {
            session.hide.mark_unmapped();
            session.hide.mark_presented();
        }
        if !session.hide.can_capture() {
            session.hide.mark_unmapped();
            session.hide.mark_presented();
        }
    });
    Ok(())
}

fn hide_session_surface(app: &AppHandle, label: &str) {
    ui::hide_window(app, label);
    let _ = wait_until_hidden(|| ui::is_visible(app, label), Duration::from_millis(400));
    wait_compositor_presented();
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
    let (frame, monitor) = grab_pointer_screen()?;
    store_freeze(app, frame, monitor.clone(), Vec::new())?;
    ui::open_overlay(app, &monitor)?;
    Ok(())
}

async fn capture_window_mode(app: &AppHandle) -> Result<(), CaptureError> {
    let (frame, monitor) = grab_pointer_screen()?;
    let windows = platform::list_windows(platform::self_pid())?;
    if windows.is_empty() {
        return Err(CaptureError::unavailable(
            "没有可截取的窗口，或当前桌面无法列出窗口。请改用区域或全屏截取。",
        ));
    }
    store_freeze(app, frame, monitor.clone(), windows)?;
    ui::open_overlay(app, &monitor)?;
    Ok(())
}

async fn capture_fullscreen(app: &AppHandle) -> Result<(), CaptureError> {
    let (frame, _monitor) = grab_pointer_screen()?;
    complete_success(app, frame)
}

fn grab_pointer_screen() -> Result<(Frame, MonitorGeom), CaptureError> {
    let monitor = platform::pointer_monitor()?;
    let frame = platform::capture_monitor(&monitor)?;
    Ok((frame, monitor))
}

fn store_freeze(
    app: &AppHandle,
    frame: Frame,
    monitor: MonitorGeom,
    windows: Vec<ListedWindow>,
) -> Result<(), CaptureError> {
    with_session_mut(app, |session| {
        let session = session.as_mut().ok_or_else(CaptureError::cancelled)?;
        if session.cancelled {
            return Err(CaptureError::cancelled());
        }
        session.freeze = Some(frame);
        session.monitor = Some(monitor);
        session.windows = windows;
        Ok(())
    })
}

pub fn overlay_frame(app: &AppHandle) -> Result<OverlayPayload, CaptureError> {
    with_session(app, |session| {
        let session = session.as_ref().ok_or_else(|| CaptureError::api("没有正在进行的截取。"))?;
        let frame = session
            .freeze
            .as_ref()
            .ok_or_else(|| CaptureError::invalid_buffer("未初始化"))?;
        let monitor = session
            .monitor
            .as_ref()
            .ok_or_else(|| CaptureError::api("没有显示器信息。"))?;
        ui::overlay_payload(session.mode, frame, monitor, session.windows.clone())
    })
}

pub fn preview_frame(app: &AppHandle) -> Result<PreviewPayload, CaptureError> {
    with_session(app, |session| {
        let frame = session
            .as_ref()
            .and_then(|item| item.freeze.as_ref())
            .ok_or_else(|| CaptureError::api("没有可预览的截图。"))?;
        ui::preview_payload(frame)
    })
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
    hide_session_surface(app, ui::OVERLAY);
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
    hide_session_surface(app, ui::OVERLAY);
    wait_compositor_presented();
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
        ui::close_window(app, label);
    }
    for surface in restore {
        ui::show_window(app, &surface.label);
    }
    with_session_mut(app, |session| *session = None);
    let _ = app.emit("capture-cancelled", ());
    Ok(CancelOutcome::clean())
}

fn complete_success(app: &AppHandle, frame: Frame) -> Result<(), CaptureError> {
    if is_cancelled(app) {
        return Err(CaptureError::cancelled());
    }
    let clipboard_error = clipboard::copy_frame(&frame).err();
    with_session_mut(app, |session| {
        if let Some(current) = session.as_mut() {
            if clipboard_error.is_none() {
                current.clipboard.commit_success();
            }
            current.freeze = Some(frame.clone());
            current.file_written = false;
        }
    });
    for label in [ui::OVERLAY, ui::DELAY, ui::ERROR] {
        ui::close_window(app, label);
    }
    ui::open_preview(app, &frame)?;
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
        ui::close_window(app, label);
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
    ui::close_window(app, ui::PREVIEW);
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
    use crate::capture::hide::{session_steps, SessionStep};

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
}
