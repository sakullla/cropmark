use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::annotate::{rasterize, Annotation};
use crate::capture::buffer::{decode_png, encode_png, Frame};
use crate::capture::error::CaptureError;
use crate::capture::session;
use crate::i18n;

/// 同时存在的贴图上限:标签 pin-1..pin-N 轮转复用空闲槽位。
pub const PIN_MAX: usize = 8;
pub const PIN_LABEL_PREFIX: &str = "pin-";
/// 满员时 `open_pin` 与 Quiet Pin Toast 共用的说明。
pub fn pin_full_message() -> String {
    i18n::t("pin.full")
}
fn pin_retry_message() -> String {
    i18n::t("pin.retry")
}


/// 轮转游标:下一次分配从上一次分配槽位之后开始找空闲标签。
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// 贴图源内容(R9):窗口存活期间常驻,供复制/保存/旋转/透明度/再标注共用;
/// 关闭或窗口销毁即随槽位清空。逻辑尺寸记录窗口 1x 基准,旋转后随之换向。
#[derive(Debug, Clone)]
struct PinSource {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    scale: f64,
    logical_width: f64,
    logical_height: f64,
}

/// 按槽位保存的源图仓库;取代旧的 take-once 交接邮箱。
static STORE: Mutex<[Option<PinSource>; PIN_MAX]> =
    Mutex::new([None, None, None, None, None, None, None, None]);

fn with_store<R>(f: impl FnOnce(&mut [Option<PinSource>; PIN_MAX]) -> R) -> R {
    let mut guard = STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

pub fn slot_label(slot: usize) -> String {
    format!("{PIN_LABEL_PREFIX}{}", slot + 1)
}

/// 仅接受 pin-1..pin-N;其它标签(含 pin-0/pin-99)一律拒绝。
pub fn slot_from_label(label: &str) -> Option<usize> {
    let rest = label.strip_prefix(PIN_LABEL_PREFIX)?;
    let slot: usize = rest.parse().ok()?;
    if (1..=PIN_MAX).contains(&slot) {
        Some(slot - 1)
    } else {
        None
    }
}

/// 纯分配逻辑(可单测):从游标起轮转找第一个空闲槽,全部占用返回 None。
fn pick_slot(cursor: usize, occupied: impl Fn(usize) -> bool) -> Option<usize> {
    for step in 0..PIN_MAX {
        let slot = (cursor + step) % PIN_MAX;
        if !occupied(slot) {
            return Some(slot);
        }
    }
    None
}

fn fail(err: CaptureError) -> String {
    let message = err.user_message();
    if message.is_empty() {
        i18n::t("error.pin.failed")
    } else {
        message
    }
}

/// 角度归一化:只接受 90° 的整数倍,其余按整除取模(前端只发 0/90/180/270)。
fn quarter_turns(rotation: u32) -> u32 {
    (rotation / 90) % 4
}

/// 顺时针旋转 90°:像素 (x, y) → (h-1-y, x),宽高互换。
fn rotate_frame_cw(frame: &Frame) -> Frame {
    let (width, height) = (frame.width as usize, frame.height as usize);
    if width == 0 || height == 0 {
        return frame.clone();
    }
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let src = (y * width + x) * 4;
            let dst = (x * height + (height - 1 - y)) * 4;
            rgba[dst..dst + 4].copy_from_slice(&frame.rgba[src..src + 4]);
        }
    }
    Frame {
        width: frame.height,
        height: frame.width,
        rgba,
        scale: frame.scale,
    }
}

/// 透明度:仅乘算 alpha 通道,颜色保持不变;1.0 直接返回副本。
fn apply_opacity(frame: &Frame, opacity: f32) -> Frame {
    let opacity = opacity.clamp(0.0, 1.0);
    let mut out = frame.clone();
    if opacity >= 1.0 {
        return out;
    }
    for pixel in out.rgba.chunks_exact_mut(4) {
        pixel[3] = ((pixel[3] as f32) * opacity).round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// 当前显示内容:源图先按旋转角旋转,再乘透明度;复制/保存/再标注共用。
fn transformed_frame(source: &PinSource, rotation: u32, opacity: f32) -> Frame {
    let mut frame = Frame {
        width: source.width,
        height: source.height,
        rgba: source.rgba.clone(),
        scale: source.scale,
    };
    for _ in 0..quarter_turns(rotation) {
        frame = rotate_frame_cw(&frame);
    }
    apply_opacity(&frame, opacity)
}

fn source_frame(source: &PinSource) -> Frame {
    Frame {
        width: source.width,
        height: source.height,
        rgba: source.rgba.clone(),
        scale: source.scale,
    }
}

/// 回写:渲染结果替换源像素;旋转 90°/270° 会交换像素朝向,逻辑宽高同步换向,
/// 保证窗口 1x 基准与图像方向一致。
fn replace_source_content(source: &mut PinSource, rendered: Frame) {
    let swapped = source.width != rendered.width || source.height != rendered.height;
    source.rgba = rendered.rgba;
    source.width = rendered.width;
    source.height = rendered.height;
    source.scale = rendered.scale;
    if swapped {
        std::mem::swap(&mut source.logical_width, &mut source.logical_height);
    }
}

/// 保存路径统一为 PNG 后缀:无扩展名追加,其它扩展名替换,避免内容与名称不符。
fn ensure_png_extension(path: PathBuf) -> PathBuf {
    let is_png = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
    if is_png {
        return path;
    }
    let mut adjusted = path;
    adjusted.set_extension("png");
    adjusted
}

type LogicalPoint = (f64, f64);
type LogicalRect = (f64, f64, f64, f64);

/// 贴图窗口原点(纯逻辑,可单测):以光标为中心,钳制在工作区内;
/// 无光标信息时取工作区中心,无显示器信息时退到固定边距。
fn pin_origin(
    cursor: Option<LogicalPoint>,
    work: Option<LogicalRect>,
    width: f64,
    height: f64,
) -> LogicalPoint {
    const MARGIN: f64 = 24.0;
    match (cursor, work) {
        (Some((cx, cy)), Some((wx, wy, ww, wh))) => (
            clamp_origin(cx - width / 2.0, wx, ww - width),
            clamp_origin(cy - height / 2.0, wy, wh - height),
        ),
        (None, Some((wx, wy, ww, wh))) => (
            wx + (ww - width).max(0.0) / 2.0,
            wy + (wh - height).max(0.0) / 2.0,
        ),
        _ => (MARGIN, MARGIN),
    }
}

/// 窗口比工作区更宽/更高时钳制区间为空(min>max),此时贴齐原点不产生负偏移。
fn clamp_origin(desired: f64, origin: f64, available: f64) -> f64 {
    if available <= 0.0 {
        origin
    } else {
        desired.clamp(origin, origin + available)
    }
}

/// 贴图初始逻辑尺寸(纯逻辑,可单测):物理帧换算为逻辑尺寸,超过工作区
/// 80% 时等比缩小,不放大。
fn pin_logical_size(width: u32, height: u32, scale: f64, work_w: f64, work_h: f64) -> (f64, f64) {
    let scale = scale.max(f64::EPSILON);
    let w = width.max(1) as f64 / scale;
    let h = height.max(1) as f64 / scale;
    let fit = (((work_w * 0.8) / w).min((work_h * 0.8) / h)).min(1.0);
    (w * fit, h * fit)
}

/// 光标逻辑坐标与光标所在显示器的工作区(逻辑)。
fn pointer_work_area(app: &AppHandle) -> (Option<LogicalPoint>, Option<LogicalRect>) {
    let Some(cursor) = app.cursor_position().ok() else {
        return (None, None);
    };
    let Some(monitor) = app
        .monitor_from_point(cursor.x, cursor.y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())
    else {
        return (None, None);
    };
    let scale = monitor.scale_factor().max(f64::EPSILON);
    let origin = monitor.position();
    let area = monitor.work_area();
    let work = (
        area.position.x as f64 / scale,
        area.position.y as f64 / scale,
        area.size.width as f64 / scale,
        area.size.height as f64 / scale,
    );
    // 光标按「显示器物理原点→逻辑」换算,再叠加工作区逻辑原点。
    let cursor_logical = (
        work.0 + (cursor.x - origin.x as f64) / scale,
        work.1 + (cursor.y - origin.y as f64) / scale,
    );
    (Some(cursor_logical), Some(work))
}

/// 启动时预建的空闲贴图窗数量。Windows 上 `WebviewWindowBuilder::build`
/// 要拉起 WebView2,点选区「贴图」时现建会卡几百毫秒到数秒;池里有窗则只
/// 换图+显示。占用以 STORE 为准,关闭只隐藏不销毁,下一次复用同一 webview。
const PIN_PRECREATE: usize = 2;

fn park_pin_window(window: &WebviewWindow) {
    let _ = window.set_ignore_cursor_events(true);
    let _ = window.set_always_on_top(false);
    let _ = window.hide();
    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize {
        width: 1.0,
        height: 1.0,
    }));
    let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition {
        x: -32000.0,
        y: -32000.0,
    }));
}

fn ensure_pin_window(app: &AppHandle, slot: usize) -> Result<WebviewWindow, String> {
    let label = slot_label(slot);
    if let Some(window) = app.get_webview_window(&label) {
        return Ok(window);
    }
    let window = WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::App("index.html?view=pin".into()),
    )
    .title("Cropmark")
    .decorations(false)
    .shadow(true)
    .skip_taskbar(true)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .always_on_top(false)
    .visible(false)
    .inner_size(1.0, 1.0)
    .build()
    .map_err(|error| i18n::tp("error.pin.window_create", &[("error", &error.to_string())]))?;
    park_pin_window(&window);
    Ok(window)
}

/// 启动期预建空闲贴图窗,让第一次选区贴图不必现拉 WebView2。
pub fn precreate(app: &AppHandle) {
    for slot in 0..PIN_PRECREATE {
        if let Err(error) = ensure_pin_window(app, slot) {
            eprintln!("Cropmark: 预创建贴图窗口失败 slot={slot}: {error}");
        }
    }
}

/// 打开一张贴图:按 STORE 占用轮转空闲槽;窗口尽量复用预建/已关闭的 webview。
/// 源图存入 STORE,前端经 `pin-reload` 再拉 `get_pin_image`。
pub fn open_pin(
    app: &AppHandle,
    frame: Frame,
    logical_width: f64,
    logical_height: f64,
) -> Result<WebviewWindow, String> {
    let cursor = NEXT_SLOT.load(Ordering::SeqCst);
    let slot = with_store(|slots| pick_slot(cursor, |slot| slots[slot].is_some()))
        .ok_or_else(pin_full_message)?;
    NEXT_SLOT.store((slot + 1) % PIN_MAX, Ordering::SeqCst);

    let (cursor_pos, work) = pointer_work_area(app);
    let (width, height) = (logical_width.max(1.0), logical_height.max(1.0));
    let (x, y) = pin_origin(cursor_pos, work, width, height);

    with_store(|slots| {
        slots[slot] = Some(PinSource {
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
            scale: frame.scale,
            logical_width: width,
            logical_height: height,
        });
    });
    let window = match ensure_pin_window(app, slot) {
        Ok(window) => window,
        Err(error) => {
            with_store(|slots| slots[slot] = None);
            return Err(error);
        }
    };
    // 先定位再显示,避免窗口在左上角闪现后再跳到光标附近。
    let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }));
    let _ = window.set_ignore_cursor_events(false);
    let _ = window.set_always_on_top(true);
    let _ = window.emit("pin-reload", ());
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    Ok(window)
}

/// 截取开始:贴图不要盖在原生选区上面,否则看起来像「选区出不来」。
pub fn lower_for_capture(app: &AppHandle) {
    for slot in 0..PIN_MAX {
        if let Some(window) = app.get_webview_window(&slot_label(slot)) {
            let _ = window.set_always_on_top(false);
        }
    }
}

/// 截取结束:把仍在用的贴图重新置顶。
pub fn restore_after_capture(app: &AppHandle) {
    for slot in 0..PIN_MAX {
        let in_use = with_store(|slots| slots[slot].is_some());
        if !in_use {
            continue;
        }
        if let Some(window) = app.get_webview_window(&slot_label(slot)) {
            if window.is_visible().unwrap_or(false) {
                let _ = window.set_always_on_top(true);
            }
        }
    }
}

/// 从已存 PNG(历史记录等外部入口)打开贴图:按工作区适配窗口逻辑尺寸,
/// 复用与预览贴图相同的 `pin_logical_size` 规则;解码后的 RGBA 存入 STORE。
pub fn open_pin_from_frame(
    app: &AppHandle,
    png: Vec<u8>,
    width: u32,
    height: u32,
    scale: f64,
) -> Result<(), String> {
    let mut frame = decode_png(&png).map_err(fail)?;
    // 索引尺寸只作核对;两者不一致(外部改动文件等)时以 PNG 实际为准,
    // 避免窗口基准与图像比例错位。
    if frame.width != width.max(1) || frame.height != height.max(1) {
        eprintln!(
            "Cropmark: 历史贴图尺寸 {}x{} 与索引 {width}x{height} 不一致，按文件尺寸显示。",
            frame.width, frame.height
        );
    }
    frame.scale = scale;
    let (_, work) = pointer_work_area(app);
    let (work_w, work_h) = work.map(|(.., w, h)| (w, h)).unwrap_or((1920.0, 1080.0));
    let (logical_w, logical_h) = pin_logical_size(frame.width, frame.height, scale, work_w, work_h);
    open_pin(app, frame, logical_w, logical_h).map(|_| ())
}

struct PreparedPin {
    frame: Frame,
    width: f64,
    height: f64,
}

/// 从会话保留帧(预览帧或 Quiet TTL 帧)合成帧与窗口尺寸;不含建窗。
fn prepare_pin(app: &AppHandle, annotations: &[Annotation]) -> Result<PreparedPin, String> {
    let frame = session::current_preview_frame(app).map_err(fail)?;
    let rendered = if annotations.is_empty() {
        frame
    } else {
        rasterize(&frame, annotations).map_err(fail)?
    };
    let (_, work) = pointer_work_area(app);
    let (work_w, work_h) = work.map(|(.., w, h)| (w, h)).unwrap_or((1920.0, 1080.0));
    let (width, height) =
        pin_logical_size(rendered.width, rendered.height, rendered.scale, work_w, work_h);
    Ok(PreparedPin {
        frame: rendered,
        width,
        height,
    })
}

/// 预览工具条「贴图」:必须是 async。Windows 上同步 command 占主线程,
/// WebviewWindowBuilder::build 要泵 WebView2 消息,会和 invoke 死锁,
/// 预览停在「正在贴图…」且整个 UI 卡死。
#[tauri::command]
pub async fn pin_current(app: AppHandle, annotations: Vec<Annotation>) -> Result<(), String> {
    let prepared = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || prepare_pin(&app, &annotations)
    })
    .await
    .map_err(|_| i18n::t("error.pin.thread_pin"))??;
    open_pin(&app, prepared.frame, prepared.width, prepared.height).map(|_| ())
}

/// Quiet Pin 失败 Toast:保留 `pin_current` 的可读原因(含满 8 张说明);
/// 空串才退回泛化重试文案。
fn quiet_pin_toast(message: &str) -> String {
    if message.is_empty() {
        pin_retry_message()
    } else {
        message.to_string()
    }
}

/// Quiet Pin 动作入口(选区操作条 Pin:不经前端、不带标注)。
/// 供 capture 动作分发接线(capture/mod.rs `run_quiet_action` 的 Pin 分支,
/// 归 shell-wiring 任务):成功无提示(贴图窗即反馈),失败走 toast。
#[allow(dead_code)]
pub fn pin_retained(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(message) = pin_current(app.clone(), Vec::new()).await {
            let toast = quiet_pin_toast(&message);
            crate::capture::ui::show_toast(&app, &toast);
        }
    });
}

/// 读取槽位源图(克隆);窗口已关闭或源图已清时返回错误文案。
fn source_for(label: &str) -> Result<PinSource, String> {
    let slot = slot_from_label(label).ok_or_else(|| i18n::t("error.pin.window_unknown"))?;
    with_store(|slots| slots[slot].clone())
        .ok_or_else(|| i18n::t("error.pin.source_gone"))
}

/// 读取源图并应用当前旋转/透明度(复制、保存、再标注共用)。
fn transformed_frame_for(
    app: &AppHandle,
    label: &str,
    rotation: u32,
    opacity: f32,
) -> Result<Frame, String> {
    if app.get_webview_window(label).is_none() {
        return Err(i18n::t("error.pin.window_closed"));
    }
    let source = source_for(label)?;
    Ok(transformed_frame(&source, rotation, opacity))
}

/// 前端拉取本窗口的源图 PNG;STORE 常驻保留,供复制/保存/旋转/再标注复用。
#[tauri::command]
pub fn get_pin_image(app: AppHandle, label: String) -> Result<tauri::ipc::Response, String> {
    if app.get_webview_window(&label).is_none() {
        return Err(i18n::t("error.pin.window_closed"));
    }
    let source = source_for(&label)?;
    let png = encode_png(&source_frame(&source)).map_err(fail)?;
    Ok(tauri::ipc::Response::new(png))
}

/// 复制当前显示内容(源图 + 旋转 + 透明度)为无损 PNG。
#[tauri::command]
pub async fn copy_pin(
    app: AppHandle,
    label: String,
    rotation: u32,
    opacity: f32,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || {
            let frame = transformed_frame_for(&app, &label, rotation, opacity)?;
            let png = encode_png(&frame).map_err(fail)?;
            crate::clipboard::copy_frame_with_png(&frame, &png).map_err(fail)
        }
    })
    .await
    .map_err(|_| i18n::t("error.pin.thread_copy"))?
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PinSaveResult {
    pub saved: bool,
    /// 已写入文件的完整路径;取消时为 None。
    pub path: Option<String>,
}

/// 保存当前显示内容为 PNG:一次编码写盘,不经过标注/导出管线。
#[tauri::command]
pub async fn save_pin(
    app: AppHandle,
    label: String,
    rotation: u32,
    opacity: f32,
) -> Result<PinSaveResult, String> {
    let frame = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        let label = label.clone();
        move || transformed_frame_for(&app, &label, rotation, opacity)
    })
    .await
    .map_err(|_| i18n::t("error.pin.thread_save"))??;

    let mut dialog = rfd::AsyncFileDialog::new()
        .add_filter(i18n::t("dialog.png_filter"), &["png"])
        .set_file_name(crate::export::default_pin_file_name())
        .set_title(i18n::t("dialog.save_pin_title"));
    if let Some(directory) = crate::settings::current_export(&app).existing_directory() {
        dialog = dialog.set_directory(directory);
    }
    if let Some(window) = app.get_webview_window(&label) {
        dialog = dialog.set_parent(&window);
    }
    let Some(file) = dialog.save_file().await else {
        return Ok(PinSaveResult {
            saved: false,
            path: None,
        });
    };
    let path = ensure_png_extension(file.path().to_path_buf());
    let bytes = tauri::async_runtime::spawn_blocking(move || encode_png(&frame).map_err(fail))
        .await
        .map_err(|_| i18n::t("error.pin.thread_save"))??;
    std::fs::write(&path, bytes)
        .map_err(|error| i18n::tp("error.pin.save_to_path", &[("path", &path.display().to_string()), ("error", &error.to_string())]))?;
    Ok(PinSaveResult {
        saved: true,
        path: Some(path.to_string_lossy().into_owned()),
    })
}

/// 贴图再标注(R9):把当前显示内容(旋转/透明度已应用)装入预览会话,
/// 并标记回写目标 label;确认/取消由 `update_pin_from_preview` 与预览
/// 关闭路径收尾。打开期间来源贴图取消置顶,避免盖住编辑器。
#[tauri::command]
pub async fn begin_pin_edit(
    app: AppHandle,
    label: String,
    rotation: u32,
    opacity: f32,
) -> Result<(), String> {
    let frame = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        let label = label.clone();
        move || transformed_frame_for(&app, &label, rotation, opacity)
    })
    .await
    .map_err(|_| i18n::t("error.pin.thread_pin"))??;

    let window = app.get_webview_window(&label);
    // 切换再标注目标时先把上一个来源贴图恢复置顶,避免遗留非置顶窗口。
    if let Some(previous) = session::writeback_target(&app) {
        if previous != label {
            if let Some(previous) = app.get_webview_window(&previous) {
                let _ = previous.set_always_on_top(true);
            }
        }
    }
    if let Some(window) = window.as_ref() {
        let _ = window.set_always_on_top(false);
    }
    if let Err(error) = crate::capture::open_pin_edit_preview(&app, frame, label) {
        if let Some(window) = window.as_ref() {
            let _ = window.set_always_on_top(true);
        }
        return Err(fail(error));
    }
    Ok(())
}

/// 预览侧查询当前是否处于贴图再标注模式;返回回写目标 label。
#[tauri::command]
pub fn get_pin_writeback(app: AppHandle) -> Option<String> {
    session::writeback_target(&app)
}

/// 再标注确认:把预览帧 + 标注栅格化后写回贴图源,通知贴图窗换图并关闭预览。
/// 取消(预览直接关闭)不调用本命令,贴图内容保持不变。
#[tauri::command]
pub async fn update_pin_from_preview(
    app: AppHandle,
    annotations: Vec<Annotation>,
) -> Result<(), String> {
    let Some(label) = session::writeback_target(&app) else {
        return Err(i18n::t("error.pin.not_editing"));
    };
    let frame = session::current_preview_frame(&app).map_err(fail)?;
    let rendered = tauri::async_runtime::spawn_blocking(move || {
        rasterize(&frame, &annotations).map_err(fail)
    })
    .await
    .map_err(|_| i18n::t("error.pin.thread_pin"))??;

    let slot = slot_from_label(&label).ok_or_else(|| i18n::t("error.pin.window_unknown"))?;
    let stored = with_store(|slots| {
        let Some(source) = slots[slot].as_mut() else {
            return Err(i18n::t("error.pin.closed"));
        };
        replace_source_content(source, rendered);
        Ok(())
    });
    stored?;

    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.set_always_on_top(true);
        let _ = window.emit("pin-reload", ());
    }
    session::close_preview(&app);
    Ok(())
}

/// 预览关闭/新截取开始时的贴图收尾:恢复再标注来源贴图的置顶;
/// 不改贴图内容(取消语义)。
pub fn finish_pin_edit(app: &AppHandle) {
    let Some(label) = session::writeback_target(app) else {
        return;
    };
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.set_always_on_top(true);
    }
}

#[tauri::command]
pub fn close_pin(app: AppHandle, label: String) {
    if let Some(slot) = slot_from_label(&label) {
        with_store(|slots| slots[slot] = None);
    }
    if let Some(window) = app.get_webview_window(&label) {
        park_pin_window(&window);
    }
}

#[tauri::command]
pub fn close_all_pins(app: AppHandle) {
    close_all(&app);
}

/// 退出清理:遍历关闭全部贴图窗口(标签占用随窗口销毁自动释放)。
pub fn close_all(app: &AppHandle) {
    for slot in 0..PIN_MAX {
        with_store(|slots| slots[slot] = None);
        if let Some(window) = app.get_webview_window(&slot_label(slot)) {
            let _ = window.close();
        }
    }
}

/// 窗口销毁(手动关闭/显示器断开/应用退出)时清掉源图仓库,
/// 防止已关闭贴图的 RGBA 滞留在 Rust 侧。
pub fn handle_destroyed(label: &str) {
    if let Some(slot) = slot_from_label(label) {
        with_store(|slots| slots[slot] = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip_for_slots_1_to_8() {
        for slot in 0..PIN_MAX {
            assert_eq!(slot_from_label(&slot_label(slot)), Some(slot));
        }
        assert_eq!(slot_from_label("pin-0"), None);
        assert_eq!(slot_from_label("pin-9"), None);
        assert_eq!(slot_from_label("pin-x"), None);
        assert_eq!(slot_from_label("preview"), None);
        assert_eq!(slot_from_label("pin-"), None);
    }

    #[test]
    fn pick_slot_rotates_from_cursor() {
        let all_free = |_| false;
        // 全空闲:从游标处取槽。
        assert_eq!(pick_slot(0, all_free), Some(0));
        assert_eq!(pick_slot(3, all_free), Some(3));
        assert_eq!(pick_slot(PIN_MAX, all_free), Some(0));
    }

    #[test]
    fn pick_slot_reuses_freed_labels_round_robin() {
        // 游标 3,槽 3..=7 被占用:轮转到 0。
        assert_eq!(pick_slot(3, |slot| (3..8).contains(&slot)), Some(0));
        // 游标 0,只有 0 号占用:顺延到 1。
        assert_eq!(pick_slot(0, |slot| slot == 0), Some(1));
    }

    #[test]
    fn pick_slot_treats_cleared_store_as_idle_even_if_window_kept() {
        // 关闭贴图只清 STORE、窗口仍在:下一张必须能拿到同一槽。
        let mut store = [true, false, false, false, false, false, false, false];
        assert_eq!(pick_slot(0, |slot| store[slot]), Some(1));
        store[1] = true;
        store[0] = false;
        assert_eq!(pick_slot(0, |slot| store[slot]), Some(0));
    }

    #[test]
    fn pick_slot_reports_exhaustion_at_cap() {
        let all_occupied = |_| true;
        for cursor in 0..PIN_MAX {
            assert_eq!(pick_slot(cursor, all_occupied), None);
        }
    }

    #[test]
    fn pin_origin_centers_on_cursor_inside_work_area() {
        let (x, y) = pin_origin(
            Some((960.0, 500.0)),
            Some((0.0, 0.0, 1920.0, 1080.0)),
            400.0,
            300.0,
        );
        assert_eq!(x, 960.0 - 200.0);
        assert_eq!(y, 500.0 - 150.0);
    }

    #[test]
    fn pin_origin_clamps_into_work_area() {
        // 光标贴左上角:窗口不越过工作区原点。
        let (x, y) = pin_origin(
            Some((10.0, 10.0)),
            Some((100.0, 50.0, 1920.0, 1080.0)),
            400.0,
            300.0,
        );
        assert_eq!((x, y), (100.0, 50.0));
        // 光标贴右下角:窗口贴工作区右/下缘。
        let (x, y) = pin_origin(
            Some((2000.0, 1100.0)),
            Some((100.0, 50.0, 1920.0, 1080.0)),
            400.0,
            300.0,
        );
        assert_eq!((x, y), (100.0 + 1920.0 - 400.0, 50.0 + 1080.0 - 300.0));
    }

    #[test]
    fn pin_origin_falls_back_to_center_or_margin() {
        // 无光标:工作区中心。
        let (x, y) = pin_origin(None, Some((0.0, 0.0, 1920.0, 1080.0)), 400.0, 300.0);
        assert_eq!(x, (1920.0 - 400.0) / 2.0);
        assert_eq!(y, (1080.0 - 300.0) / 2.0);
        // 无显示器信息:固定边距。
        assert_eq!(
            pin_origin(Some((5.0, 5.0)), None, 400.0, 300.0),
            (24.0, 24.0)
        );
        assert_eq!(pin_origin(None, None, 400.0, 300.0), (24.0, 24.0));
        // 窗口大于工作区:贴齐原点,不产生负偏移。
        let (x, y) = pin_origin(
            Some((10.0, 10.0)),
            Some((50.0, 40.0, 300.0, 200.0)),
            400.0,
            300.0,
        );
        assert_eq!((x, y), (50.0, 40.0));
    }

    #[test]
    fn pin_logical_size_uses_frame_scale_without_upscale() {
        // 2x 屏上的 800x600 物理帧:逻辑 400x300。
        let (w, h) = pin_logical_size(800, 600, 2.0, 1920.0, 1080.0);
        assert!((w - 400.0).abs() < 1e-9 && (h - 300.0).abs() < 1e-9);
        // 小图不放大。
        let (w, h) = pin_logical_size(100, 100, 1.0, 1920.0, 1080.0);
        assert!((w - 100.0).abs() < 1e-9 && (h - 100.0).abs() < 1e-9);
    }

    #[test]
    fn pin_logical_size_fits_work_area_keeping_aspect() {
        let (w, h) = pin_logical_size(7680, 4320, 1.0, 1920.0, 1080.0);
        assert!(w <= 1920.0 * 0.8 + 1e-9);
        assert!(h <= 1080.0 * 0.8 + 1e-9);
        assert!((w / h - 7680.0 / 4320.0).abs() < 0.01);
        // 0 倍防御:非法 scale 不至于除零崩溃。
        let (w, h) = pin_logical_size(10, 10, 0.0, 1920.0, 1080.0);
        assert!(w.is_finite() && h.is_finite());
    }

    #[test]
    fn quiet_pin_toast_surfaces_eight_slot_notice() {
        let full = pin_full_message();
        assert_eq!(quiet_pin_toast(&full), full);
        assert!(full.contains("8"));
    }

    #[test]
    fn quiet_pin_toast_keeps_other_readable_errors() {
        assert_eq!(quiet_pin_toast("无法创建贴图窗口：timeout"), "无法创建贴图窗口：timeout");
        assert_eq!(quiet_pin_toast("贴图线程失败。"), "贴图线程失败。");
        assert_eq!(quiet_pin_toast(""), pin_retry_message());
    }

    #[test]
    fn quarter_turns_normalizes_to_rotation_steps() {
        assert_eq!(quarter_turns(0), 0);
        assert_eq!(quarter_turns(90), 1);
        assert_eq!(quarter_turns(270), 3);
        assert_eq!(quarter_turns(360), 0);
        assert_eq!(quarter_turns(450), 1);
    }

    #[test]
    fn rotate_frame_cw_maps_pixels_and_swaps_dimensions() {
        // 2x1 左红右蓝 → 顺时针 90° 后 1x2 上红下蓝。
        let frame = Frame {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 0, 255, 255],
            scale: 1.0,
        };
        let rotated = rotate_frame_cw(&frame);
        assert_eq!((rotated.width, rotated.height), (1, 2));
        assert_eq!(&rotated.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&rotated.rgba[4..8], &[0, 0, 255, 255]);
    }

    #[test]
    fn rotate_frame_four_quarters_round_trips() {
        let frame = Frame {
            width: 3,
            height: 2,
            rgba: (0..24).collect(),
            scale: 2.0,
        };
        let mut rotated = frame.clone();
        for _ in 0..4 {
            rotated = rotate_frame_cw(&rotated);
        }
        assert_eq!((rotated.width, rotated.height), (3, 2));
        assert_eq!(rotated.rgba, frame.rgba);
        assert_eq!(rotated.scale, frame.scale);
    }

    #[test]
    fn opacity_scales_alpha_without_touching_color() {
        let frame = Frame {
            width: 2,
            height: 1,
            rgba: vec![10, 20, 30, 200, 1, 2, 3, 255],
            scale: 1.0,
        };
        let half = apply_opacity(&frame, 0.5);
        assert_eq!(&half.rgba[0..3], &[10, 20, 30]);
        assert_eq!(half.rgba[3], 100);
        // 255 * 0.5 = 127.5 → 128。
        assert_eq!(half.rgba[7], 128);
        assert_eq!(apply_opacity(&frame, 1.0).rgba, frame.rgba);
        let none = apply_opacity(&frame, 0.0);
        assert_eq!(none.rgba[3], 0);
        assert_eq!(none.rgba[7], 0);
        // 越界值按 0..=1 钳制。
        assert_eq!(apply_opacity(&frame, -1.0).rgba[3], 0);
        assert_eq!(apply_opacity(&frame, 2.0).rgba, frame.rgba);
    }

    #[test]
    fn transformed_frame_applies_rotation_then_opacity() {
        let source = PinSource {
            rgba: vec![255, 0, 0, 200, 0, 0, 255, 200],
            width: 2,
            height: 1,
            scale: 1.5,
            logical_width: 40.0,
            logical_height: 20.0,
        };
        let frame = transformed_frame(&source, 90, 0.5);
        assert_eq!((frame.width, frame.height), (1, 2));
        assert_eq!(frame.scale, 1.5);
        assert_eq!(&frame.rgba[0..3], &[255, 0, 0]);
        assert_eq!(frame.rgba[3], 100);
        assert_eq!(frame.rgba[7], 100);
        // 非法角度按整除归零,不越界 panic。
        let plain = transformed_frame(&source, 45, 1.0);
        assert_eq!((plain.width, plain.height), (2, 1));
    }

    #[test]
    fn replace_source_content_swaps_logical_size_when_orientation_changes() {
        let mut source = PinSource {
            rgba: vec![0; 8],
            width: 2,
            height: 1,
            scale: 1.0,
            logical_width: 200.0,
            logical_height: 100.0,
        };
        let rendered = Frame {
            width: 1,
            height: 2,
            rgba: vec![1; 8],
            scale: 1.0,
        };
        replace_source_content(&mut source, rendered);
        assert_eq!((source.width, source.height), (1, 2));
        assert_eq!(source.logical_width, 100.0);
        assert_eq!(source.logical_height, 200.0);
        // 同取向回写不交换。
        let same = Frame {
            width: 1,
            height: 2,
            rgba: vec![2; 8],
            scale: 1.0,
        };
        replace_source_content(&mut source, same);
        assert_eq!(source.logical_width, 100.0);
        assert_eq!(source.logical_height, 200.0);
    }

    #[test]
    fn ensure_png_extension_normalizes_names() {
        assert_eq!(
            ensure_png_extension(PathBuf::from("shot.png")),
            PathBuf::from("shot.png")
        );
        assert_eq!(
            ensure_png_extension(PathBuf::from("shot.PNG")),
            PathBuf::from("shot.PNG")
        );
        assert_eq!(
            ensure_png_extension(PathBuf::from("shot.jpg")),
            PathBuf::from("shot.png")
        );
        assert_eq!(
            ensure_png_extension(PathBuf::from("shot")),
            PathBuf::from("shot.png")
        );
    }

    #[test]
    fn source_frame_keeps_native_pixels_and_scale() {
        let source = PinSource {
            rgba: vec![9; 12],
            width: 1,
            height: 3,
            scale: 2.0,
            logical_width: 10.0,
            logical_height: 30.0,
        };
        let frame = source_frame(&source);
        assert_eq!((frame.width, frame.height), (1, 3));
        assert_eq!(frame.rgba, source.rgba);
        assert_eq!(frame.scale, 2.0);
    }
}
