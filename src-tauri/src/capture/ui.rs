use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, PhysicalPosition, Position, Size, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder,
};

use super::buffer::{fit_display, Frame};
use super::error::CaptureError;
use super::geometry::MonitorGeom;
use super::windows_list::ListedWindow;
use crate::annotate::Annotation;
use crate::hotkeys::CaptureMode;
use crate::i18n;

pub const OVERLAY: &str = "overlay";
pub const PREVIEW: &str = "preview";
pub const DELAY: &str = "capture-delay";
pub const ERROR: &str = "capture-error";
pub const SETTINGS: &str = "settings";
pub const HISTORY: &str = "history";
pub const TOAST: &str = "toast";

const TOAST_WIDTH: f64 = 300.0;
const TOAST_HEIGHT: f64 = 48.0;
const TOAST_MARGIN: f64 = 24.0;
const TOAST_DURATION: Duration = Duration::from_millis(1800);

/// 最近一次 toast 的来源:词条键+参数可按当前语言重新解析(语言切换后
/// 前端重拉仍显示正确文案);不透明系统文案按原样保留。
#[derive(Debug, Clone)]
enum ToastSource {
    Key {
        key: String,
        params: Vec<(String, String)>,
    },
    Text(String),
}

impl ToastSource {
    fn resolve(&self) -> String {
        match self {
            Self::Key { key, params } => {
                let borrowed: Vec<(&str, &str)> = params
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .collect();
                i18n::tp(key, &borrowed)
            }
            Self::Text(text) => text.clone(),
        }
    }
}

static LAST_TOAST: Mutex<Option<ToastSource>> = Mutex::new(None);
static TOAST_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToastPayload {
    pub message: String,
}

/// R24:Web 覆盖层能力子集。只携带覆盖层实际会消费的开关(Wayland 的标注
/// 入口/提示据此渲染);新增字段不影响旧前端——前端按结构类型读取已知字段,
/// 未知字段天然忽略,不要求与后端同版本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayCapabilities {
    /// 选区即时标注:关闭后覆盖层不提供标注入口。
    pub inline_annotation: bool,
}

impl OverlayCapabilities {
    /// 从功能设置取覆盖层能力子集;后续覆盖层能力在此扩展。
    pub fn from_features(features: crate::settings::FeatureSettings) -> Self {
        Self {
            inline_annotation: features.inline_annotation,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayPayload {
    pub mode: CaptureMode,
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub logical_width: u32,
    pub logical_height: u32,
    /// R13:当前覆盖层缺少原生选区壳能力(操作条/放大镜/取色/微调)时为
    /// true,前端据此展示不可用能力说明与替代方式,不伪造这些功能。
    pub reduced_capabilities: bool,
    /// R24:能力子集;旧前端忽略未知字段不受影响。
    pub capabilities: OverlayCapabilities,
    pub windows: Vec<ListedWindow>,
}

#[derive(Debug, Clone)]
pub struct PreviewPayload {
    pub bytes: Vec<u8>,
}

/// 预览头部的复制状态:0=自动复制被设置关闭,1=已复制,2=自动复制失败。
/// 三个状态分开后,预览首条提示不会把"用户主动关闭"误报为失败(R4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewCopyState {
    Disabled,
    Copied,
    Failed,
}

impl PreviewCopyState {
    pub fn code(self) -> u32 {
        match self {
            Self::Disabled => 0,
            Self::Copied => 1,
            Self::Failed => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelayPayload {
    pub delay_ms: u64,
    pub mode: CaptureMode,
}

pub fn product_window_labels() -> [&'static str; 3] {
    [PREVIEW, SETTINGS, HISTORY]
}

pub fn session_window_labels() -> [&'static str; 3] {
    [OVERLAY, DELAY, ERROR]
}

pub fn hide_window(app: &AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        if label == OVERLAY {
            dismiss_session_overlay_window(&window);
        } else {
            let _ = window.set_always_on_top(false);
            let _ = window.hide();
        }
    }
    crate::front::demote_if_idle(app);
}

pub fn show_window(app: &AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        if label == PREVIEW {
            present_preview_window(&window, app);
        } else if label == SETTINGS || label == HISTORY || label == ERROR {
            crate::front::reveal(app, &window);
        } else {
            let _ = window.set_ignore_cursor_events(false);
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}

pub fn close_window(app: &AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.close();
    }
}

pub fn is_visible(app: &AppHandle, label: &str) -> bool {
    app.get_webview_window(label)
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
}

pub fn any_visible(app: &AppHandle, labels: &[&str]) -> bool {
    labels.iter().any(|label| is_visible(app, label))
}

pub fn overlay_payload(
    mode: CaptureMode,
    frame: &Frame,
    monitor: &MonitorGeom,
    windows: Vec<ListedWindow>,
    reduced_capabilities: bool,
    capabilities: OverlayCapabilities,
) -> Result<OverlayPayload, CaptureError> {
    let windows = windows
        .into_iter()
        .map(|mut window| {
            window.x -= monitor.physical_x;
            window.y -= monitor.physical_y;
            window
        })
        .collect();
    let (width, height) = fit_display(monitor.logical_width, monitor.logical_height, 1280);
    let overlay = super::buffer::resize_rgba(frame, width, height)?;
    Ok(OverlayPayload {
        mode,
        png_base64: STANDARD.encode(super::buffer::encode_jpeg(&overlay, 70)?),
        width: frame.width,
        height: frame.height,
        scale: frame.scale,
        logical_width: monitor.logical_width,
        logical_height: monitor.logical_height,
        reduced_capabilities,
        capabilities,
        windows,
    })
}

pub fn preview_payload(
    frame: &Frame,
    png: &[u8],
    copy: PreviewCopyState,
    annotations: &[Annotation],
) -> PreviewPayload {
    // One binary response keeps metadata and pixels bound to the same capture.
    // Header: width/height (u32), scale (f64), copy state (u32), annotations JSON
    // length (u32), little-endian, then the annotation JSON, then the PNG.
    // R21:选区即时标注随帧进入预览编辑器(可继续编辑/撤销);空列表编码为 `[]`。
    let annotations = serde_json::to_vec(annotations).unwrap_or_else(|_| b"[]".to_vec());
    let mut bytes = Vec::with_capacity(24 + annotations.len() + png.len());
    bytes.extend_from_slice(&frame.width.to_le_bytes());
    bytes.extend_from_slice(&frame.height.to_le_bytes());
    bytes.extend_from_slice(&frame.scale.to_le_bytes());
    bytes.extend_from_slice(&copy.code().to_le_bytes());
    bytes.extend_from_slice(&(annotations.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&annotations);
    bytes.extend_from_slice(png);
    PreviewPayload { bytes }
}

pub fn precreate(app: &AppHandle) {
    let _ = ensure_window(app, OVERLAY, "overlay", 320.0, 240.0, false, true);
    // 预创建的 overlay 默认 always-on-top;立刻停放到屏外,避免隐藏态仍命中点击。
    dismiss_session_overlay(app);
    let _ = ensure_window(app, PREVIEW, "preview", 520.0, 360.0, false, false);
    if let Some(window) = app.get_webview_window(PREVIEW) {
        let _ = window.set_always_on_top(false);
        let _ = window.set_ignore_cursor_events(false);
    }
    // toast/error 预创建复用:这两个窗每次 close+create 重建时,新 webview
    // 偶发导航失败显示"无法访问此页面"(协议宿主竞态);预创建后仅 show/hide。
    let _ = ensure_window(app, TOAST, "toast", TOAST_WIDTH, TOAST_HEIGHT, true, true);
    let _ = ensure_window(app, ERROR, "error", 420.0, 268.0, true, true);
}

pub fn open_overlay(app: &AppHandle, monitor: &MonitorGeom) -> Result<WebviewWindow, CaptureError> {
    let window = ensure_window(app, OVERLAY, "overlay", 320.0, 240.0, false, true)?;
    let _ = window.set_position(Position::Physical(PhysicalPosition {
        x: monitor.physical_x,
        y: monitor.physical_y,
    }));
    let _ = window.set_size(Size::Logical(LogicalSize {
        width: monitor.logical_width.max(1) as f64,
        height: monitor.logical_height.max(1) as f64,
    }));
    let _ = window.set_ignore_cursor_events(false);
    let _ = window.set_always_on_top(true);
    let _ = window.show();
    let _ = window.set_focus();
    let _ = window.emit("overlay-reload", ());
    Ok(window)
}

pub fn open_preview(app: &AppHandle, frame: &Frame) -> Result<WebviewWindow, CaptureError> {
    let area = target_work_area(app);
    let (work_w, work_h) = area.map(|(.., w, h)| (w, h)).unwrap_or((1920.0, 1080.0));
    let (width, height) = preview_size(frame, work_w, work_h);
    let window = ensure_window(app, PREVIEW, "preview", width, height, false, false)?;
    let _ = window.set_size(Size::Logical(LogicalSize { width, height }));
    if let Some((x, y, ..)) = area {
        let _ = window.set_position(Position::Logical(LogicalPosition {
            x: centered_offset(x, work_w, width),
            y: centered_offset(y, work_h, height),
        }));
    } else {
        let _ = window.center();
    }
    let _ = window.set_skip_taskbar(false);
    present_preview_window(&window, app);
    let _ = window.emit("preview-reload", ());
    Ok(window)
}

pub fn open_delay(app: &AppHandle, delay_ms: u64) -> Result<WebviewWindow, CaptureError> {
    close_window(app, DELAY);
    let window = builder(app, DELAY, "delay", true, true)?
        .inner_size(280.0, 88.0)
        .always_on_top(true)
        .focused(false)
        .visible(true)
        .center()
        .build()
        .map_err(|error| CaptureError::api_detail("error.capture.window_build", &error.to_string()))?;
    let _ = window.emit("capture-delay", DelayPayload { delay_ms, mode: CaptureMode::Region });
    Ok(window)
}

pub fn open_error(app: &AppHandle, error: &CaptureError) -> Result<(), CaptureError> {
    // 复用预创建的 error 窗,避免重建 webview 的偶发导航失败。
    let window = ensure_window(app, ERROR, "error", 420.0, 268.0, true, true)?;
    let _ = window.center();
    crate::front::reveal(app, &window);
    let _ = window.emit("capture-error", error.clone());
    Ok(())
}

/// Latest toast text, so a freshly created toast view can catch up even if it
/// missed the live event (same pattern as delay/error views). 词条键按当前
/// 语言重新解析,语言切换后重拉不会残留旧语言。
pub fn toast_message() -> Option<String> {
    LAST_TOAST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(ToastSource::resolve)
}

/// Transient result feedback: one borderless topmost window, replaced on every
/// call, auto-closed after ~2s. Never steals focus.
pub fn show_toast(app: &AppHandle, message: &str) {
    show_toast_source(app, ToastSource::Text(message.to_string()), Some(TOAST_DURATION));
}

/// 词条键形式的结果反馈:语言切换后重拉 toast 时按新语言解析。
pub fn show_toast_key(app: &AppHandle, key: &str) {
    show_toast_source(app, ToastSource::Key { key: key.into(), params: Vec::new() }, Some(TOAST_DURATION));
}

/// 带 `{name}` 占位符参数的词条 toast。
pub fn show_toast_key_params(app: &AppHandle, key: &str, params: &[(&str, &str)]) {
    show_toast_source(
        app,
        ToastSource::Key {
            key: key.into(),
            params: params
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
        },
        Some(TOAST_DURATION),
    );
}

/// 词条键形式的进行中提示(不自动消失):静默取字首次加载模型时保持可见,
/// 直到结果到达被下一条 toast 替换(R11)。
pub fn show_progress_toast_key(app: &AppHandle, key: &str) {
    show_toast_source(app, ToastSource::Key { key: key.into(), params: Vec::new() }, None);
}

fn show_toast_source(app: &AppHandle, source: ToastSource, auto_hide: Option<Duration>) {
    let message = source.resolve().trim().to_string();
    if message.is_empty() {
        return;
    }
    *LAST_TOAST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(source);
    // Only the newest toast may hide the window; older timers become no-ops.
    let generation = TOAST_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    // 复用预创建的 toast 窗:仅重定位+显示+发消息,不重建 webview。
    let window = ensure_window(app, TOAST, "toast", TOAST_WIDTH, TOAST_HEIGHT, true, true);
    if let Ok(window) = window {
        let (x, y) = toast_origin(target_work_area(app), TOAST_WIDTH, TOAST_HEIGHT);
        let _ = window.set_position(Position::Logical(LogicalPosition { x, y }));
        let _ = window.show();
        let _ = window.emit("capture-toast", ToastPayload { message });
    }
    let Some(duration) = auto_hide else {
        return;
    };
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(duration)).await;
        if TOAST_GENERATION.load(Ordering::SeqCst) == generation {
            hide_window(&handle, TOAST);
        }
    });
}

fn toast_origin(area: Option<(f64, f64, f64, f64)>, width: f64, height: f64) -> (f64, f64) {
    match area {
        Some((origin_x, origin_y, area_w, area_h)) => (
            (origin_x + area_w - width - TOAST_MARGIN).max(origin_x + TOAST_MARGIN),
            (origin_y + area_h - height - TOAST_MARGIN).max(origin_y + TOAST_MARGIN),
        ),
        None => (TOAST_MARGIN, TOAST_MARGIN),
    }
}

/// 预览先置顶抬到前台拿到焦点,再取消 always-on-top。
/// 这样用户能看见结果,也能切到其它应用;覆盖层在焦点到手后再停放,
/// 避免 hide overlay 把前台让给资源管理器后预览点不了按钮。
fn present_preview_window(window: &WebviewWindow, app: &AppHandle) {
    let _ = window.set_ignore_cursor_events(false);
    let _ = window.set_always_on_top(true);
    crate::front::reveal(app, window);
    dismiss_session_overlay(app);
    // overlay hide 可能再次把前台让出去;停放后再抢一次,再摘置顶。
    // 预览在任务栏,摘置顶后可切到其它应用;贴图窗才保持置顶。
    crate::front::reveal(app, window);
    let _ = window.set_always_on_top(false);
}

fn dismiss_session_overlay(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(OVERLAY) {
        dismiss_session_overlay_window(&window);
    }
}

fn dismiss_session_overlay_window(window: &WebviewWindow) {
    let _ = window.set_ignore_cursor_events(true);
    let _ = window.set_always_on_top(false);
    let _ = window.hide();
    let (pos, size) = overlay_park_placement();
    let _ = window.set_size(Size::Logical(size));
    let _ = window.set_position(Position::Logical(pos));
}

/// 隐藏失败时也不要盖住桌面:1×1 停在屏外。
fn overlay_park_placement() -> (LogicalPosition<f64>, LogicalSize<f64>) {
    (
        LogicalPosition {
            x: -32000.0,
            y: -32000.0,
        },
        LogicalSize {
            width: 1.0,
            height: 1.0,
        },
    )
}

/// Windows 前台锁:热键/覆盖层 hide 之后 SetForegroundWindow 常被拒绝,
/// 预览会变成“看得见但点不到”的置顶窗。把前台线程输入队列临时挂过来。
#[cfg(windows)]
fn force_foreground(window: &WebviewWindow) {
    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    unsafe { force_foreground_hwnd(hwnd) };
}

/// # Safety
/// `hwnd` 必须是仍有效的预览窗句柄。
#[cfg(windows)]
unsafe fn force_foreground_hwnd(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
        ShowWindow, SW_RESTORE,
    };

    let _ = ShowWindow(hwnd, SW_RESTORE);
    if SetForegroundWindow(hwnd).as_bool() {
        return;
    }
    let fg = GetForegroundWindow();
    if fg == hwnd || fg == HWND::default() {
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        return;
    }
    let fg_thread = GetWindowThreadProcessId(fg, None);
    let this_thread = GetCurrentThreadId();
    if fg_thread != 0 && fg_thread != this_thread {
        let _ = AttachThreadInput(fg_thread, this_thread, true);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = AttachThreadInput(fg_thread, this_thread, false);
    } else {
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn ensure_window(
    app: &AppHandle,
    label: &str,
    view: &str,
    width: f64,
    height: f64,
    transparent: bool,
    skip_taskbar: bool,
) -> Result<WebviewWindow, CaptureError> {
    if let Some(window) = app.get_webview_window(label) {
        return Ok(window);
    }
    builder(app, label, view, transparent, skip_taskbar)?
        .inner_size(width, height)
        .visible(false)
        .always_on_top(true)
        .visible_on_all_workspaces(label == OVERLAY)
        .build()
        .map_err(|error| CaptureError::api_detail("error.capture.window_build", &error.to_string()))
}

fn builder<'a>(
    app: &'a AppHandle,
    label: &str,
    view: &str,
    transparent: bool,
    skip_taskbar: bool,
) -> Result<WebviewWindowBuilder<'a, tauri::Wry, AppHandle>, CaptureError> {
    Ok(WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::App(format!("index.html?view={view}").into()),
    )
    .title("Cropmark")
    .decorations(false)
    .transparent(transparent)
    .shadow(!transparent)
    .skip_taskbar(skip_taskbar)
    .resizable(label == PREVIEW)
    .maximizable(false)
    .minimizable(false)
    .closable(true))
}

fn target_work_area(app: &AppHandle) -> Option<(f64, f64, f64, f64)> {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|position| app.monitor_from_point(position.x, position.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let area = monitor.work_area();
    let scale = monitor.scale_factor().max(f64::EPSILON);
    Some((
        area.position.x as f64 / scale,
        area.position.y as f64 / scale,
        area.size.width as f64 / scale,
        area.size.height as f64 / scale,
    ))
}

fn centered_offset(origin: f64, area: f64, window: f64) -> f64 {
    origin + (area - window).max(0.0) / 2.0
}

fn preview_size(frame: &Frame, work_w: f64, work_h: f64) -> (f64, f64) {
    let max_w = (work_w * 0.9).max(1.0);
    let max_h = (work_h * 0.9).max(1.0);
    let chrome = 108.0;
    let min_w = 480.0_f64.min(max_w);
    let min_h = 280.0_f64.min(max_h);
    let width = frame.width.max(1) as f64;
    let height = frame.height.max(1) as f64;
    let scale = (max_w / width)
        .min(((max_h - chrome).max(1.0)) / height)
        .min(1.0);
    (
        (width * scale).max(min_w).min(max_w),
        (height * scale + chrome).max(min_h).min(max_h),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::Frame;

    /// 头部之后的 JSON 长度与 PNG 起点(测试共用)。
    fn payload_parts(bytes: &[u8]) -> (usize, &[u8], &[u8]) {
        let json_len = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
        let json = &bytes[24..24 + json_len];
        let png = &bytes[24 + json_len..];
        (json_len, json, png)
    }

    #[test]
    fn binary_preview_preserves_native_size_scale_and_original_png() {
        let frame = Frame { width: 2, height: 1, rgba: vec![1, 2, 3, 255, 4, 5, 6, 128], scale: 1.5 };
        let png = crate::capture::buffer::encode_png(&frame).unwrap();
        let payload = preview_payload(&frame, &png, PreviewCopyState::Copied, &[]);
        assert_eq!(u32::from_le_bytes(payload.bytes[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(payload.bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(f64::from_le_bytes(payload.bytes[8..16].try_into().unwrap()), 1.5);
        assert_eq!(u32::from_le_bytes(payload.bytes[16..20].try_into().unwrap()), 1);
        let (json_len, json, png_bytes) = payload_parts(&payload.bytes);
        assert_eq!(json, b"[]");
        assert!(json_len > 0);
        assert_eq!(png_bytes, png.as_slice());
        assert_eq!(
            crate::capture::buffer::decode_png(png_bytes).unwrap().rgba,
            frame.rgba
        );
    }

    #[test]
    fn preview_header_carries_inline_annotations_as_camel_case_json() {
        use crate::annotate::{Annotation, Point};
        let frame = Frame {
            width: 1,
            height: 1,
            rgba: vec![1, 1, 1, 255],
            scale: 1.0,
        };
        let png = crate::capture::buffer::encode_png(&frame).unwrap();
        let annotations = vec![Annotation::Arrow {
            from: Point { x: 1.0, y: 2.0 },
            to: Point { x: 9.0, y: 8.0 },
            color: "#e11d48".into(),
            stroke_width: None,
        }];
        let payload = preview_payload(&frame, &png, PreviewCopyState::Copied, &annotations);
        let (_, json, png_bytes) = payload_parts(&payload.bytes);
        let parsed: Vec<Annotation> = serde_json::from_slice(json).unwrap();
        assert_eq!(parsed, annotations);
        assert!(std::str::from_utf8(json).unwrap().contains("\"type\":\"arrow\""));
        assert_eq!(png_bytes, png.as_slice());
    }

    #[test]
    fn preview_header_distinguishes_disabled_from_failed_auto_copy() {
        let frame = Frame {
            width: 1,
            height: 1,
            rgba: vec![9, 9, 9, 255],
            scale: 1.0,
        };
        let png = crate::capture::buffer::encode_png(&frame).unwrap();
        let code = |state: PreviewCopyState| {
            let payload = preview_payload(&frame, &png, state, &[]);
            u32::from_le_bytes(payload.bytes[16..20].try_into().unwrap())
        };
        assert_eq!(code(PreviewCopyState::Disabled), 0);
        assert_eq!(code(PreviewCopyState::Copied), 1);
        assert_eq!(code(PreviewCopyState::Failed), 2);
        let payload = preview_payload(&frame, &png, PreviewCopyState::Disabled, &[]);
        let (_, _, png_bytes) = payload_parts(&payload.bytes);
        assert_eq!(png_bytes, png.as_slice());
    }

    fn frame(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            rgba: vec![0; 4],
            scale: 2.0,
        }
    }

    #[test]
    fn preview_fits_1080p_work_area() {
        let (width, height) = preview_size(&frame(3840, 2160), 1920.0, 1080.0);
        assert!(width <= 1920.0 * 0.9);
        assert!(height <= 1080.0 * 0.9);
        let ratio = 3840.0 / 2160.0;
        assert!((width / (height - 108.0) - ratio).abs() < 0.01);
    }

    #[test]
    fn preview_fits_1366_work_area() {
        let (width, height) = preview_size(&frame(3840, 2160), 1366.0, 720.0);
        assert!(width <= 1366.0 * 0.9);
        assert!(height <= 720.0 * 0.9);
        let ratio = 3840.0 / 2160.0;
        assert!((width / (height - 108.0) - ratio).abs() < 0.01);
    }

    #[test]
    fn preview_scales_up_to_4k_work_area() {
        let (width, height) = preview_size(&frame(3840, 2160), 3840.0, 2160.0);
        assert!(width <= 3840.0 * 0.9);
        assert!(height <= 2160.0 * 0.9);
        assert!(width > 800.0);
        let ratio = 3840.0 / 2160.0;
        assert!((width / (height - 108.0) - ratio).abs() < 0.01);
    }

    #[test]
    fn preview_enforces_minimum_panel() {
        let (width, height) = preview_size(&frame(100, 100), 1920.0, 1080.0);
        assert_eq!(width, 480.0);
        assert_eq!(height, 280.0);
    }

    #[test]
    fn centered_offset_centers_inside_target_work_area() {
        assert_eq!(centered_offset(100.0, 1920.0, 800.0), 100.0 + 560.0);
        assert_eq!(centered_offset(0.0, 500.0, 480.0), 10.0);
        // 窗口大于工作区时贴齐原点,不产生负偏移
        assert_eq!(centered_offset(50.0, 400.0, 600.0), 50.0);
    }

    #[test]
    fn preview_minimum_yields_to_tiny_work_area() {        let (width, height) = preview_size(&frame(100, 100), 500.0, 400.0);
        assert!(width <= 500.0 * 0.9);
        assert!(height <= 400.0 * 0.9);
    }

    #[test]
    fn preview_small_frame_keeps_native_size() {
        let (width, height) = preview_size(&frame(640, 400), 1920.0, 1080.0);
        assert_eq!(width, 640.0);
        assert_eq!(height, 400.0 + 108.0);
    }

    #[test]
    fn toast_hugs_work_area_bottom_right() {
        let (x, y) = toast_origin(Some((100.0, 50.0, 1920.0, 1080.0)), TOAST_WIDTH, TOAST_HEIGHT);
        assert_eq!(x, 100.0 + 1920.0 - TOAST_WIDTH - 24.0);
        assert_eq!(y, 50.0 + 1080.0 - TOAST_HEIGHT - 24.0);
    }

    #[test]
    fn toast_never_leaves_work_area_or_goes_negative() {
        let (x, y) = toast_origin(Some((0.0, 0.0, 200.0, 40.0)), TOAST_WIDTH, TOAST_HEIGHT);
        assert_eq!((x, y), (24.0, 24.0));
        let (x, y) = toast_origin(None, TOAST_WIDTH, TOAST_HEIGHT);
        assert_eq!((x, y), (24.0, 24.0));
    }

    #[test]
    fn overlay_park_is_off_screen_and_tiny() {
        let (pos, size) = overlay_park_placement();
        assert!(pos.x <= -10000.0);
        assert!(pos.y <= -10000.0);
        assert_eq!(size.width, 1.0);
        assert_eq!(size.height, 1.0);
    }

    #[test]
    fn overlay_payload_serializes_reduced_capability_flag_for_frontend() {
        let frame = Frame {
            width: 4,
            height: 4,
            rgba: vec![9; 64],
            scale: 1.0,
        };
        let monitor = MonitorGeom::from_physical("m", 0, 0, 4, 4, 1.0);
        let capabilities =
            OverlayCapabilities::from_features(crate::settings::FeatureSettings::default());
        // R13:前端按 `reducedCapabilities` 渲染能力说明,字段名必须是 camelCase。
        let reduced = overlay_payload(
            CaptureMode::Region,
            &frame,
            &monitor,
            Vec::new(),
            true,
            capabilities,
        )
        .expect("payload builds");
        assert!(reduced.reduced_capabilities);
        let json = serde_json::to_value(&reduced).expect("payload serializes");
        assert_eq!(json["reducedCapabilities"], serde_json::json!(true));
        // R24:能力子集字段名同样为 camelCase。
        assert_eq!(json["capabilities"]["inlineAnnotation"], serde_json::json!(true));

        let full = overlay_payload(
            CaptureMode::Window,
            &frame,
            &monitor,
            Vec::new(),
            false,
            capabilities,
        )
        .expect("payload builds");
        assert!(!full.reduced_capabilities);
    }

    #[test]
    fn overlay_capabilities_track_inline_annotation_setting() {
        let on = OverlayCapabilities::from_features(crate::settings::FeatureSettings::default());
        assert!(on.inline_annotation);
        let off = OverlayCapabilities::from_features(crate::settings::FeatureSettings {
            inline_annotation: false,
            ..crate::settings::FeatureSettings::default()
        });
        assert!(!off.inline_annotation);
        let json = serde_json::to_value(off).expect("capabilities serialize");
        assert_eq!(json["inlineAnnotation"], serde_json::json!(false));
        // 子集只序列化已声明字段,不夹带其它设置。
        assert_eq!(json.as_object().map(|map| map.len()), Some(1));
    }

    #[test]
    fn overlay_payload_capability_fields_do_not_break_older_consumers() {
        // R24:"忽略未知字段":旧覆盖层只认识能力字段之前的普通形状,
        // 新增 capabilities 不应导致其反序列化失败。
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct LegacyOverlayFrame {
            mode: CaptureMode,
            png_base64: String,
            width: u32,
            height: u32,
            scale: f64,
            logical_width: u32,
            logical_height: u32,
            reduced_capabilities: bool,
            windows: Vec<serde_json::Value>,
        }
        let frame = Frame {
            width: 4,
            height: 4,
            rgba: vec![9; 64],
            scale: 1.0,
        };
        let monitor = MonitorGeom::from_physical("m", 0, 0, 4, 4, 1.0);
        let payload = overlay_payload(
            CaptureMode::Region,
            &frame,
            &monitor,
            Vec::new(),
            true,
            OverlayCapabilities::from_features(crate::settings::FeatureSettings {
                inline_annotation: false,
                ..crate::settings::FeatureSettings::default()
            }),
        )
        .expect("payload builds");
        let json = serde_json::to_value(&payload).expect("payload serializes");
        let legacy: LegacyOverlayFrame =
            serde_json::from_value(json).expect("older consumer ignores unknown fields");
        assert_eq!(legacy.mode, CaptureMode::Region);
        assert!(!legacy.png_base64.is_empty());
        assert_eq!(legacy.width, 4);
        assert_eq!(legacy.height, 4);
        assert_eq!(legacy.scale, 1.0);
        assert_eq!(legacy.logical_width, 4);
        assert_eq!(legacy.logical_height, 4);
        assert!(legacy.reduced_capabilities);
        assert!(legacy.windows.is_empty());
    }
}
