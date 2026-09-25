use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

use crate::annotate::{rasterize, Annotation};
use crate::capture::buffer::{decode_png, encode_png, Frame};
use crate::capture::error::CaptureError;
use crate::capture::session;
use crate::i18n;
use crate::pin_store::{self, PinRecord};
use crate::settings;

/// 同时存在的贴图上限:标签 pin-1..pin-N 轮转复用空闲槽位。
pub const PIN_MAX: usize = 8;
pub const PIN_LABEL_PREFIX: &str = "pin-";
/// 缩放上下限:与前端滚轮步进一致,越界在 Rust 侧钳制。
pub const MIN_ZOOM: f64 = 0.2;
pub const MAX_ZOOM: f64 = 5.0;
/// 透明度下限:0 会让贴图完全不可见且难以再选中。
pub const MIN_OPACITY: f32 = pin_store::MIN_PIN_OPACITY;
/// 几何/变换变更的合并写盘窗口。
const PERSIST_DEBOUNCE: Duration = Duration::from_millis(400);
/// 运行时显示器布局检查间隔。
const MONITOR_WATCH_INTERVAL: Duration = Duration::from_millis(1500);
/// 小于该值的位置变化视为回执噪声(窗口移动事件与自身移动互相触发)。
const MOVE_EPSILON: f64 = 0.5;
/// 每格滚轮的缩放倍率。
const ZOOM_STEP: f64 = 1.1;

/// 满员时 `open_pin` 与 Quiet Pin Toast 共用的说明。
pub fn pin_full_message() -> String {
    i18n::t("pin.full")
}

fn pin_retry_message() -> String {
    i18n::t("pin.retry")
}

/// 轮转游标:下一次分配从上一次分配槽位之后开始找空闲标签。
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);
/// 贴图与分组 id 的进程内递增序号(与毫秒时间戳组合,避免槽位复用撞名)。
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// 贴图源内容与增强状态(R2):源图常驻供复制/保存/旋转/翻转/透明度/再标注共用;
/// 旋转、翻转、透明度、几何与分组由 Rust 持有并持久化,前端只做显示同步。
#[derive(Debug, Clone)]
struct PinEntry {
    id: String,
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    scale: f64,
    /// 当前旋转取向下 1x 的逻辑基准尺寸(窗口尺寸 = 基准 × 缩放)。
    logical_width: f64,
    logical_height: f64,
    /// 窗口逻辑位置与尺寸,随移动/缩放/旋转更新并持久化。
    position: (f64, f64),
    window_width: f64,
    window_height: f64,
    rotation: u32,
    flip_h: bool,
    flip_v: bool,
    opacity: f32,
    group: Option<String>,
    /// 穿透是会话内状态:不持久化,重启后一律恢复为可交互。
    click_through: bool,
    /// 内容版本自增,用于判断哪些贴图的 PNG 需要重写。
    content_seq: u64,
    /// 已落盘的 content_seq。
    persisted_seq: u64,
}

impl PinEntry {
    fn new(
        id: String,
        frame: Frame,
        logical_width: f64,
        logical_height: f64,
        position: (f64, f64),
    ) -> Self {
        Self {
            id,
            rgba: frame.rgba,
            width: frame.width,
            height: frame.height,
            scale: frame.scale,
            logical_width,
            logical_height,
            position,
            window_width: logical_width,
            window_height: logical_height,
            rotation: 0,
            flip_h: false,
            flip_v: false,
            opacity: 1.0,
            group: None,
            click_through: false,
            content_seq: 1,
            persisted_seq: 0,
        }
    }

    fn record(&self) -> PinRecord {
        PinRecord {
            id: self.id.clone(),
            x: self.position.0,
            y: self.position.1,
            width: self.window_width,
            height: self.window_height,
            scale: self.scale,
            rotation: self.rotation,
            flip_h: self.flip_h,
            flip_v: self.flip_v,
            opacity: self.opacity,
            group: self.group.clone(),
        }
    }

    fn zoom(&self) -> f64 {
        self.window_width / self.logical_width.max(f64::EPSILON)
    }
}

/// 按槽位保存的源图与状态仓库。
static STORE: Mutex<[Option<PinEntry>; PIN_MAX]> =
    Mutex::new([None, None, None, None, None, None, None, None]);

fn with_store<R>(f: impl FnOnce(&mut [Option<PinEntry>; PIN_MAX]) -> R) -> R {
    let mut guard = STORE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

fn with_entry<R>(label: &str, f: impl FnOnce(&mut PinEntry) -> R) -> Result<R, String> {
    let slot = slot_from_label(label).ok_or_else(|| i18n::t("error.pin.window_unknown"))?;
    with_store(|slots| match slots[slot].as_mut() {
        Some(entry) => Ok(f(entry)),
        None => Err(i18n::t("error.pin.source_gone")),
    })
}

fn entry_snapshot(label: &str) -> Result<PinEntry, String> {
    with_entry(label, |entry| entry.clone())
}

fn window_for(app: &AppHandle, label: &str) -> Result<WebviewWindow, String> {
    app.get_webview_window(label)
        .ok_or_else(|| i18n::t("error.pin.window_closed"))
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

/// lib.rs 的窗口事件只对贴图窗口走增强分支(DPI 变化钳制等)。
pub fn is_pin_label(label: &str) -> bool {
    slot_from_label(label).is_some()
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

/// id 统一为 `p<hex 毫秒>-<hex 序号>` 形态,满足持久化层 token 校验。
fn next_token(prefix: char) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let counter = NEXT_TOKEN.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}{millis:x}-{counter:x}")
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

/// 水平镜像:像素 (x, y) → (w-1-x, y)。
fn flip_frame_horizontal(frame: &Frame) -> Frame {
    let (width, height) = (frame.width as usize, frame.height as usize);
    if width == 0 || height == 0 {
        return frame.clone();
    }
    let mut out = frame.clone();
    for y in 0..height {
        for x in 0..width {
            let src = (y * width + x) * 4;
            let dst = (y * width + (width - 1 - x)) * 4;
            out.rgba[dst..dst + 4].copy_from_slice(&frame.rgba[src..src + 4]);
        }
    }
    out
}

/// 垂直镜像:整行逆序,像素 (x, y) → (x, h-1-y)。
fn flip_frame_vertical(frame: &Frame) -> Frame {
    let (width, height) = (frame.width as usize, frame.height as usize);
    if width == 0 || height == 0 {
        return frame.clone();
    }
    let mut out = frame.clone();
    for y in 0..height {
        let src_row = y * width * 4;
        let dst_row = (height - 1 - y) * width * 4;
        out.rgba[dst_row..dst_row + width * 4]
            .copy_from_slice(&frame.rgba[src_row..src_row + width * 4]);
    }
    out
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

/// 当前显示内容:源图依次应用旋转、水平/垂直翻转与透明度;
/// 画面、复制、保存、再标注共用同一变换,保证三者一致。
fn transformed_frame(entry: &PinEntry) -> Frame {
    let mut frame = source_frame(entry);
    for _ in 0..quarter_turns(entry.rotation) {
        frame = rotate_frame_cw(&frame);
    }
    if entry.flip_h {
        frame = flip_frame_horizontal(&frame);
    }
    if entry.flip_v {
        frame = flip_frame_vertical(&frame);
    }
    apply_opacity(&frame, entry.opacity)
}

/// 源图(未应用任何变换);持久化与再标注回写使用。
fn source_frame(entry: &PinEntry) -> Frame {
    Frame {
        width: entry.width,
        height: entry.height,
        rgba: entry.rgba.clone(),
        scale: entry.scale,
    }
}

/// 再标注回写:渲染结果替换源像素,变换归零(渲染帧已含旋转/翻转/透明度),
/// 逻辑基准尺寸改为渲染结果;窗口几何保持不变。
fn replace_source_content(entry: &mut PinEntry, rendered: Frame) {
    let scale = rendered.scale.max(f64::EPSILON);
    entry.logical_width = (rendered.width.max(1) as f64 / scale).max(f64::EPSILON);
    entry.logical_height = (rendered.height.max(1) as f64 / scale).max(f64::EPSILON);
    entry.rgba = rendered.rgba;
    entry.width = rendered.width;
    entry.height = rendered.height;
    entry.scale = rendered.scale;
    entry.rotation = 0;
    entry.flip_h = false;
    entry.flip_v = false;
    entry.opacity = 1.0;
    entry.content_seq += 1;
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

/// 恢复时的 1x 逻辑基准尺寸:源图按旋转取向换算,翻转不改变尺寸。
fn rotated_logical_size(width: u32, height: u32, scale: f64, rotation: u32) -> (f64, f64) {
    let scale = scale.max(f64::EPSILON);
    let w = (width.max(1) as f64 / scale).max(f64::EPSILON);
    let h = (height.max(1) as f64 / scale).max(f64::EPSILON);
    if quarter_turns(rotation) % 2 == 1 {
        (h, w)
    } else {
        (w, h)
    }
}

/// 恢复的窗口尺寸:索引缺失/非法时退回逻辑基准,避免 0 尺寸窗口。
fn sanitize_window_size(width: f64, height: f64, logical: (f64, f64)) -> (f64, f64) {
    let w = if width.is_finite() && width > 0.0 {
        width
    } else {
        logical.0
    };
    let h = if height.is_finite() && height > 0.0 {
        height
    } else {
        logical.1
    };
    (w.max(1.0), h.max(1.0))
}

/// 当前显示器工作区(逻辑坐标),与 `pointer_work_area` 同一换算规则。
fn monitor_areas(app: &AppHandle) -> Vec<LogicalRect> {
    let Ok(monitors) = app.available_monitors() else {
        return Vec::new();
    };
    monitors
        .iter()
        .map(|monitor| {
            let scale = monitor.scale_factor().max(f64::EPSILON);
            let area = monitor.work_area();
            (
                area.position.x as f64 / scale,
                area.position.y as f64 / scale,
                area.size.width as f64 / scale,
                area.size.height as f64 / scale,
            )
        })
        .collect()
}

/// 与显示器矩形的重叠面积(纯逻辑,可单测)。
fn overlap_area(position: LogicalPoint, size: (f64, f64), area: LogicalRect) -> f64 {
    let left = position.0.max(area.0);
    let top = position.1.max(area.1);
    let right = (position.0 + size.0).min(area.0 + area.2);
    let bottom = (position.1 + size.1).min(area.1 + area.3);
    (right - left).max(0.0) * (bottom - top).max(0.0)
}

/// 选目标显示器:优先与贴图重叠面积最大的,无重叠时取中心最近的。
fn pick_monitor(
    position: LogicalPoint,
    size: (f64, f64),
    areas: &[LogicalRect],
) -> Option<LogicalRect> {
    let mut best_overlap: Option<(f64, LogicalRect)> = None;
    for area in areas {
        let overlap = overlap_area(position, size, *area);
        if overlap <= 0.0 {
            continue;
        }
        let better = match best_overlap {
            Some((best, _)) => overlap > best,
            None => true,
        };
        if better {
            best_overlap = Some((overlap, *area));
        }
    }
    if let Some((_, area)) = best_overlap {
        return Some(area);
    }
    let center = (
        position.0 + size.0.max(1.0) / 2.0,
        position.1 + size.1.max(1.0) / 2.0,
    );
    let mut nearest: Option<(f64, LogicalRect)> = None;
    for area in areas {
        let area_center = (area.0 + area.2 / 2.0, area.1 + area.3 / 2.0);
        let distance = (center.0 - area_center.0).abs() + (center.1 - area_center.1).abs();
        let better = match nearest {
            Some((best, _)) => distance < best,
            None => true,
        };
        if better {
            nearest = Some((distance, *area));
        }
    }
    nearest.map(|(_, area)| area)
}

/// 显示器变化后的可见区钳制(纯逻辑,可单测):窗口大于显示器时贴齐原点,
/// 其余钳入区间,保证贴图整体回到可见区域。
fn clamp_pin_position(
    position: LogicalPoint,
    size: (f64, f64),
    areas: &[LogicalRect],
) -> LogicalPoint {
    let Some((mx, my, mw, mh)) = pick_monitor(position, size, areas) else {
        return position;
    };
    let (w, h) = (size.0.max(1.0), size.1.max(1.0));
    let x = if w >= mw {
        mx
    } else {
        position.0.clamp(mx, mx + (mw - w))
    };
    let y = if h >= mh {
        my
    } else {
        position.1.clamp(my, my + (mh - h))
    };
    (x, y)
}

/// 旋转时的窗口几何(纯逻辑,可单测):尺寸换向,以中心为不动点。
fn rotated_geometry(position: LogicalPoint, window: (f64, f64)) -> (LogicalPoint, (f64, f64)) {
    let (w, h) = window;
    (
        (position.0 + (w - h) / 2.0, position.1 + (h - w) / 2.0),
        (h, w),
    )
}

/// 缩放时的窗口几何(纯逻辑,可单测):尺寸按倍率,位置绕锚点缩放。
fn scaled_geometry(
    position: LogicalPoint,
    window: (f64, f64),
    factor: f64,
    anchor: LogicalPoint,
) -> (LogicalPoint, (f64, f64)) {
    (
        (
            anchor.0 + (position.0 - anchor.0) * factor,
            anchor.1 + (position.1 - anchor.1) * factor,
        ),
        ((window.0 * factor).max(1.0), (window.1 * factor).max(1.0)),
    )
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
    // GTK 窗未 realize 时 tao 会对 GdkWindow unwrap 崩掉。
    #[cfg(not(target_os = "linux"))]
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
    let window =
        WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html?view=pin".into()))
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
            .map_err(|error| {
                i18n::tp("error.pin.window_create", &[("error", &error.to_string())])
            })?;
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
/// 源图与状态存入 STORE,前端经 `pin-reload` 再拉 `get_pin_image`/`get_pin_state`。
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
        slots[slot] = Some(PinEntry::new(next_token('p'), frame, width, height, (x, y)));
    });
    let window = match ensure_pin_window(app, slot) {
        Ok(window) => window,
        Err(error) => {
            with_store(|slots| slots[slot] = None);
            return Err(error);
        }
    };
    // 先定位再显示,避免窗口在左上角闪现后再跳到光标附近。
    let _ = window.set_position(LogicalPosition { x, y });
    let _ = window.set_size(LogicalSize { width, height });
    let _ = window.set_ignore_cursor_events(false);
    let _ = window.set_always_on_top(true);
    let _ = window.emit("pin-reload", ());
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    schedule_persist(app);
    ensure_monitor_watcher(app);
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
    let (width, height) = pin_logical_size(
        rendered.width,
        rendered.height,
        rendered.scale,
        work_w,
        work_h,
    );
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

// ---------------------------------------------------------------------------
// 持久化(ADR-6):变更防抖写盘,关闭立即删除。
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PersistState {
    dirty: bool,
    worker: bool,
}

fn persist_slot() -> &'static (Mutex<PersistState>, Condvar) {
    static SLOT: OnceLock<(Mutex<PersistState>, Condvar)> = OnceLock::new();
    SLOT.get_or_init(|| (Mutex::new(PersistState::default()), Condvar::new()))
}

/// 功能开关关闭时不写盘:增强行为随开关消失,保存语义不残留。
fn persistence_enabled(app: &AppHandle) -> bool {
    settings::current_toggles(app).pin_enhance
}

fn schedule_persist(app: &AppHandle) {
    if !persistence_enabled(app) {
        return;
    }
    let (lock, cv) = persist_slot();
    {
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.dirty = true;
        if state.worker {
            cv.notify_all();
            return;
        }
        state.worker = true;
        cv.notify_all();
    }
    let app = app.clone();
    std::thread::spawn(move || persist_worker(app));
}

/// 防抖写盘:静默 `PERSIST_DEBOUNCE` 后落盘;期间再有变更则重新计时。
fn persist_worker(app: AppHandle) {
    loop {
        let (lock, cv) = persist_slot();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while !state.dirty {
            state = cv
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.dirty = false;
        drop(state);
        std::thread::sleep(PERSIST_DEBOUNCE);
        let dirty_again = {
            let state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.dirty
        };
        if dirty_again {
            continue;
        }
        {
            let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            state.worker = false;
        }
        persist_now(&app);
        break;
    }
}

struct PendingContent {
    id: String,
    seq: u64,
    frame: Frame,
}

fn snapshot_records() -> Vec<PinRecord> {
    with_store(|slots| slots.iter().flatten().map(PinEntry::record).collect())
}

fn snapshot_pending_content() -> Vec<PendingContent> {
    with_store(|slots| {
        slots
            .iter()
            .flatten()
            .filter(|entry| entry.persisted_seq != entry.content_seq)
            .map(|entry| PendingContent {
                id: entry.id.clone(),
                seq: entry.content_seq,
                frame: source_frame(entry),
            })
            .collect()
    })
}

/// 内容写盘成功后推进版本;期间又有内容变更的贴图保持待写状态。
fn mark_persisted(written: &[(String, u64)]) {
    with_store(|slots| {
        for entry in slots.iter_mut().flatten() {
            if let Some((_, seq)) = written.iter().find(|(id, _)| id == &entry.id) {
                if entry.content_seq == *seq {
                    entry.persisted_seq = *seq;
                }
            }
        }
    });
}

/// 立即落盘:退出前调用,避免防抖窗口内的几何/变换变化丢失。
/// 后台防抖入口在退出后不再落盘,防止清空内存后把空索引写回。
fn persist_now(app: &AppHandle) {
    if EXITING.load(Ordering::SeqCst) {
        return;
    }
    write_snapshot(app);
}

fn write_snapshot(app: &AppHandle) {
    if !persistence_enabled(app) {
        return;
    }
    let dir = pin_store::dir(app);
    // 索引与内存状态在同一把锁下快照,避免与「关闭删除」交错写回旧记录。
    let _io = pin_store::io_lock();
    let records = snapshot_records();
    let mut contents = Vec::new();
    let mut written = Vec::new();
    for item in snapshot_pending_content() {
        match encode_png(&item.frame) {
            Ok(png) => {
                contents.push((item.id.clone(), png));
                written.push((item.id, item.seq));
            }
            Err(error) => eprintln!(
                "Cropmark: 贴图内容编码失败 id={}:{}",
                item.id,
                error.user_message()
            ),
        }
    }
    match pin_store::persist_to_dir(&dir, &records, &contents) {
        Ok(()) => mark_persisted(&written),
        Err(error) => eprintln!("Cropmark: 贴图状态写入失败:{error}"),
    }
}

/// 关闭即从存储删除:移除内容 PNG 并重写索引,索引里不再出现的贴图不会
/// 因下次恢复而重现;写盘失败只记录,内存关闭语义不受影响。增强开关关闭
/// 时按索引精确剔除(不覆盖本次会话未加载的记录),避免误删其他状态。
fn delete_entries(app: &AppHandle, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let dir = pin_store::dir(app);
    if !persistence_enabled(app) && !dir.exists() {
        return;
    }
    let _io = pin_store::io_lock();
    for id in ids {
        pin_store::remove_files(&dir, id);
    }
    let records = if persistence_enabled(app) {
        snapshot_records()
    } else {
        let mut stored = pin_store::load_from_dir(&dir);
        stored.retain(|record| !ids.iter().any(|id| id == &record.id));
        stored
    };
    if let Err(error) = pin_store::write_index_to_dir(&dir, &records) {
        eprintln!("Cropmark: 贴图索引更新失败:{error}");
    }
}

/// 启动恢复(ADR-6):仅 `pin_enhance && pin_restore` 时恢复;位置按当前
/// 显示器可见区域钳制;内容缺失或超出槽位上限的记录从索引剔除;已关闭的
/// 贴图早已从索引删除,不会重现。恢复关闭时保留存储但不恢复:
/// 「重启后恢复」只控制恢复行为,不删除已打开贴图的数据。
pub fn restore_persisted(app: &AppHandle) {
    let toggles = settings::current_toggles(app);
    if !toggles.pin_enhance || !toggles.pin_restore {
        return;
    }
    let dir = pin_store::dir(app);
    let records = pin_store::load_from_dir(&dir);
    if records.is_empty() {
        return;
    }
    let areas = monitor_areas(app);
    let mut kept: Vec<PinRecord> = Vec::new();
    let mut cursor = NEXT_SLOT.load(Ordering::SeqCst);
    for record in records {
        if kept.len() >= PIN_MAX {
            break;
        }
        let Some(frame) = pin_store::read_frame_from_dir(&dir, &record) else {
            eprintln!("Cropmark: 贴图内容缺失，跳过恢复 {}", record.id);
            continue;
        };
        let Some(slot) = with_store(|slots| pick_slot(cursor, |slot| slots[slot].is_some())) else {
            break;
        };
        cursor = (slot + 1) % PIN_MAX;
        let logical =
            rotated_logical_size(frame.width, frame.height, record.scale, record.rotation);
        let size = sanitize_window_size(record.width, record.height, logical);
        let position = clamp_pin_position((record.x, record.y), size, &areas);
        with_store(|slots| {
            let mut entry = PinEntry::new(record.id.clone(), frame, logical.0, logical.1, position);
            entry.window_width = size.0;
            entry.window_height = size.1;
            entry.rotation = record.rotation;
            entry.flip_h = record.flip_h;
            entry.flip_v = record.flip_v;
            entry.opacity = record.opacity;
            entry.group = record.group.clone();
            // 内容直接来自磁盘,无需再写一次 PNG。
            entry.persisted_seq = entry.content_seq;
            slots[slot] = Some(entry);
        });
        let window = match ensure_pin_window(app, slot) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("Cropmark: 恢复贴图窗口失败:{error}");
                with_store(|slots| slots[slot] = None);
                continue;
            }
        };
        let _ = window.set_position(LogicalPosition {
            x: position.0,
            y: position.1,
        });
        let _ = window.set_size(LogicalSize {
            width: size.0,
            height: size.1,
        });
        let _ = window.set_ignore_cursor_events(false);
        let _ = window.set_always_on_top(true);
        let _ = window.emit("pin-reload", ());
        let _ = window.show();
        kept.push(PinRecord {
            x: position.0,
            y: position.1,
            width: size.0,
            height: size.1,
            ..record
        });
    }
    NEXT_SLOT.store(cursor, Ordering::SeqCst);
    let _io = pin_store::io_lock();
    if let Err(error) = pin_store::write_index_to_dir(&dir, &kept) {
        eprintln!("Cropmark: 贴图索引更新失败:{error}");
    }
    if !kept.is_empty() {
        ensure_monitor_watcher(app);
    }
}

// ---------------------------------------------------------------------------
// 显示器布局变化:回到可见区域(ADR-6)。
// ---------------------------------------------------------------------------

static WATCHER_RUNNING: AtomicBool = AtomicBool::new(false);
static MONITOR_SIGNATURE: Mutex<Option<String>> = Mutex::new(None);
/// 退出闸门:置位后后台防抖写入停止,只允许退出路径的同步落盘。
static EXITING: AtomicBool = AtomicBool::new(false);

fn monitor_signature(areas: &[LogicalRect]) -> String {
    areas
        .iter()
        .map(|area| format!("{:.0},{:.0},{:.0},{:.0}", area.0, area.1, area.2, area.3))
        .collect::<Vec<_>>()
        .join("|")
}

fn has_live_pins() -> bool {
    with_store(|slots| slots.iter().any(Option::is_some))
}

/// 有贴图时按固定间隔检查显示器布局;签名变化(分辨率/数量)才钳制一次,
/// 不干扰用户把贴图主动拖到屏幕边缘。贴图全部关闭后线程自行退出。
fn ensure_monitor_watcher(app: &AppHandle) {
    if WATCHER_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let areas = monitor_areas(app);
    if !areas.is_empty() {
        let mut signature = MONITOR_SIGNATURE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *signature = Some(monitor_signature(&areas));
    }
    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(MONITOR_WATCH_INTERVAL);
        if !has_live_pins() {
            WATCHER_RUNNING.store(false, Ordering::SeqCst);
            // 退出窗口期内若有新贴图入库,重启监视,避免漏掉布局变化。
            if has_live_pins() {
                ensure_monitor_watcher(&app);
            }
            break;
        }
        let task = app.clone();
        let _ = app.run_on_main_thread(move || watch_monitors(&task));
    });
}

fn watch_monitors(app: &AppHandle) {
    let areas = monitor_areas(app);
    if areas.is_empty() {
        return;
    }
    let changed = {
        let mut signature = MONITOR_SIGNATURE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let next = monitor_signature(&areas);
        if signature.as_deref() == Some(next.as_str()) {
            false
        } else {
            *signature = Some(next);
            true
        }
    };
    if changed {
        clamp_all_pins(app);
    }
}

/// 把所有越界贴图钳回当前显示器可见区域;已在可见区的不动。
fn clamp_all_pins(app: &AppHandle) {
    let areas = monitor_areas(app);
    if areas.is_empty() {
        return;
    }
    let mut updates: Vec<(usize, LogicalPoint)> = Vec::new();
    with_store(|slots| {
        for (slot, entry) in slots.iter_mut().enumerate() {
            let Some(entry) = entry else {
                continue;
            };
            let clamped = clamp_pin_position(
                entry.position,
                (entry.window_width, entry.window_height),
                &areas,
            );
            if (clamped.0 - entry.position.0).abs() < MOVE_EPSILON
                && (clamped.1 - entry.position.1).abs() < MOVE_EPSILON
            {
                continue;
            }
            entry.position = clamped;
            updates.push((slot, clamped));
        }
    });
    if updates.is_empty() {
        return;
    }
    for (slot, position) in &updates {
        if let Some(window) = app.get_webview_window(&slot_label(*slot)) {
            let _ = window.set_position(LogicalPosition {
                x: position.0,
                y: position.1,
            });
        }
    }
    schedule_persist(app);
}

/// DPI/缩放变化时立即钳制单张贴图(lib.rs 的窗口事件接线)。
pub fn clamp_pin_label(app: &AppHandle, label: &str) {
    let areas = monitor_areas(app);
    if areas.is_empty() {
        return;
    }
    let Some((slot, position, size)) = with_store(|slots| {
        let slot = slot_from_label(label)?;
        let entry = slots[slot].as_ref()?;
        Some((
            slot,
            entry.position,
            (entry.window_width, entry.window_height),
        ))
    }) else {
        return;
    };
    let clamped = clamp_pin_position(position, size, &areas);
    if (clamped.0 - position.0).abs() < MOVE_EPSILON
        && (clamped.1 - position.1).abs() < MOVE_EPSILON
    {
        return;
    }
    with_store(|slots| {
        if let Some(entry) = slots[slot].as_mut() {
            entry.position = clamped;
        }
    });
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.set_position(LogicalPosition {
            x: clamped.0,
            y: clamped.1,
        });
    }
    schedule_persist(app);
}

// ---------------------------------------------------------------------------
// 命令:状态、变换、穿透、编组、几何。
// ---------------------------------------------------------------------------

/// 贴图增强状态(前端只做显示同步)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PinState {
    pub label: String,
    pub rotation: u32,
    pub flip_h: bool,
    pub flip_v: bool,
    pub opacity: f32,
    /// 组内成员数大于 1 时才算编组(联动与整组关闭都以此为准)。
    pub grouped: bool,
    pub group_size: usize,
    pub click_through: bool,
    pub logical_width: f64,
    pub logical_height: f64,
    pub window_width: f64,
    pub window_height: f64,
}

/// 入口可用性:功能开关、平台能力与托盘退出路径。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PinOptions {
    pub enhance: bool,
    pub restore: bool,
    pub click_through_supported: bool,
    pub click_through_reason: Option<String>,
    pub tray_available: bool,
}

fn group_size(slots: &[Option<PinEntry>; PIN_MAX], group: &Option<String>) -> usize {
    let Some(group) = group.as_deref() else {
        return 0;
    };
    slots
        .iter()
        .flatten()
        .filter(|entry| entry.group.as_deref() == Some(group))
        .count()
}

fn state_from(slots: &[Option<PinEntry>; PIN_MAX], label: &str, entry: &PinEntry) -> PinState {
    let members = group_size(slots, &entry.group);
    PinState {
        label: label.to_string(),
        rotation: entry.rotation,
        flip_h: entry.flip_h,
        flip_v: entry.flip_v,
        opacity: entry.opacity,
        grouped: members > 1,
        group_size: members,
        click_through: entry.click_through,
        logical_width: entry.logical_width,
        logical_height: entry.logical_height,
        window_width: entry.window_width,
        window_height: entry.window_height,
    }
}

fn state_snapshot_for(label: &str) -> Option<PinState> {
    with_store(|slots| {
        let slot = slot_from_label(label)?;
        let entry = slots[slot].as_ref()?;
        Some(state_from(slots, label, entry))
    })
}

fn state_for(label: &str) -> Result<PinState, String> {
    state_snapshot_for(label).ok_or_else(|| i18n::t("error.pin.source_gone"))
}

fn emit_reload(app: &AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.emit("pin-reload", ());
    }
}

/// 广播状态变化(编组/穿透/几何):只更新 UI 状态,不重拉图像。
fn broadcast_state(app: &AppHandle) {
    for slot in 0..PIN_MAX {
        let label = slot_label(slot);
        let Some(state) = state_snapshot_for(&label) else {
            continue;
        };
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.emit("pin-state", state);
        }
    }
}

/// 读取本窗口源图按当前变换处理后的 PNG:画面、复制、保存共用同一变换。
#[tauri::command]
pub fn get_pin_image(app: AppHandle, label: String) -> Result<tauri::ipc::Response, String> {
    if app.get_webview_window(&label).is_none() {
        return Err(i18n::t("error.pin.window_closed"));
    }
    let entry = entry_snapshot(&label)?;
    let png = encode_png(&transformed_frame(&entry)).map_err(fail)?;
    Ok(tauri::ipc::Response::new(png))
}

#[tauri::command]
pub fn get_pin_state(app: AppHandle, label: String) -> Result<PinState, String> {
    window_for(&app, &label)?;
    state_for(&label)
}

#[tauri::command]
pub fn get_pin_options(app: AppHandle) -> PinOptions {
    let toggles = settings::current_toggles(&app);
    let supported = click_through_supported();
    let tray = tray_available(&app);
    let reason = if !supported {
        Some(i18n::t("pin.click_through.unsupported"))
    } else if !tray {
        Some(i18n::t("pin.click_through.no_tray"))
    } else {
        None
    };
    PinOptions {
        enhance: toggles.pin_enhance,
        restore: toggles.pin_restore,
        click_through_supported: supported,
        click_through_reason: reason,
        tray_available: tray,
    }
}

/// 顺时针旋转 90°:状态与窗口尺寸同步换向,画面经 `pin-reload` 重拉。
#[tauri::command]
pub fn rotate_pin(app: AppHandle, label: String) -> Result<PinState, String> {
    let window = window_for(&app, &label)?;
    let (position, size) = with_entry(&label, |entry| {
        let (position, size) =
            rotated_geometry(entry.position, (entry.window_width, entry.window_height));
        entry.rotation = (entry.rotation + 90) % 360;
        std::mem::swap(&mut entry.logical_width, &mut entry.logical_height);
        entry.position = position;
        entry.window_width = size.0;
        entry.window_height = size.1;
        (position, size)
    })?;
    let _ = window.set_position(LogicalPosition {
        x: position.0,
        y: position.1,
    });
    let _ = window.set_size(LogicalSize {
        width: size.0,
        height: size.1,
    });
    schedule_persist(&app);
    emit_reload(&app, &label);
    state_for(&label)
}

/// 水平/垂直翻转:画面、复制与保存都走同一变换。
#[tauri::command]
pub fn flip_pin(app: AppHandle, label: String, axis: String) -> Result<PinState, String> {
    if !settings::current_toggles(&app).pin_enhance {
        return Err(i18n::t("pin.error.disabled"));
    }
    let horizontal = match axis.as_str() {
        "horizontal" | "h" => true,
        "vertical" | "v" => false,
        _ => return Err(i18n::t("pin.error.flip")),
    };
    window_for(&app, &label)?;
    with_entry(&label, |entry| {
        if horizontal {
            entry.flip_h = !entry.flip_h;
        } else {
            entry.flip_v = !entry.flip_v;
        }
    })?;
    schedule_persist(&app);
    emit_reload(&app, &label);
    state_for(&label)
}

#[tauri::command]
pub fn set_pin_opacity(app: AppHandle, label: String, opacity: f64) -> Result<PinState, String> {
    window_for(&app, &label)?;
    let opacity = (opacity as f32).clamp(MIN_OPACITY, 1.0);
    with_entry(&label, |entry| entry.opacity = opacity)?;
    schedule_persist(&app);
    emit_reload(&app, &label);
    state_for(&label)
}

/// 平台能力:Linux 仅在 X11 会话下可用(GTK 输入区在 Wayland 无对应语义);
/// Windows/macOS 走原生窗口属性。
#[cfg(target_os = "linux")]
fn wayland_session(xdg_session_type: Option<&str>, wayland_display: bool) -> bool {
    wayland_display
        || xdg_session_type.is_some_and(|value| value.trim().eq_ignore_ascii_case("wayland"))
}

pub fn click_through_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        !wayland_session(
            std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
            std::env::var_os("WAYLAND_DISPLAY").is_some(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// 托盘不可用时不得进入无法退出的穿透状态(退出入口在托盘菜单)。
fn tray_available(app: &AppHandle) -> bool {
    let state = app.state::<settings::SessionState>();
    let tray = state
        .tray
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    tray.available
}

pub fn has_click_through() -> bool {
    with_store(|slots| slots.iter().flatten().any(|entry| entry.click_through))
}

/// 切换点击穿透:平台不支持或托盘不可用时拒绝并给出原因。
#[tauri::command]
pub fn set_pin_click_through(
    app: AppHandle,
    label: String,
    enabled: bool,
) -> Result<PinState, String> {
    if !settings::current_toggles(&app).pin_enhance {
        return Err(i18n::t("pin.error.disabled"));
    }
    if enabled && !click_through_supported() {
        return Err(i18n::t("pin.click_through.unsupported"));
    }
    if enabled && !tray_available(&app) {
        return Err(i18n::t("pin.click_through.no_tray"));
    }
    let window = window_for(&app, &label)?;
    window
        .set_ignore_cursor_events(enabled)
        .map_err(|error| i18n::tp("error.pin.click_through", &[("error", &error.to_string())]))?;
    with_entry(&label, |entry| entry.click_through = enabled)?;
    crate::tray::refresh_menu(&app);
    state_for(&label)
}

/// 托盘「退出贴图穿透」:任一贴图穿透时必达的全局退出路径。
#[tauri::command]
pub fn exit_pin_click_through(app: AppHandle) {
    let mut changed = false;
    for slot in 0..PIN_MAX {
        let label = slot_label(slot);
        let was = with_store(|slots| {
            slots[slot]
                .as_mut()
                .map(|entry| std::mem::replace(&mut entry.click_through, false))
                .unwrap_or(false)
        });
        if !was {
            continue;
        }
        changed = true;
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.set_ignore_cursor_events(false);
        }
    }
    if changed {
        broadcast_state(&app);
        crate::tray::refresh_menu(&app);
    }
}

/// 编组当前全部贴图(至少两张);组 id 由后端生成并随索引持久化。
#[tauri::command]
pub fn group_all_pins(app: AppHandle) -> Result<usize, String> {
    if !settings::current_toggles(&app).pin_enhance {
        return Err(i18n::t("pin.error.disabled"));
    }
    let count = with_store(|slots| slots.iter().filter(|entry| entry.is_some()).count());
    if count < 2 {
        return Err(i18n::t("pin.error.group_need_two"));
    }
    let group = next_token('g');
    with_store(|slots| {
        for entry in slots.iter_mut().flatten() {
            entry.group = Some(group.clone());
        }
    });
    schedule_persist(&app);
    broadcast_state(&app);
    Ok(count)
}

/// 解组当前贴图;只剩单个成员的组自动消散,便于单独关闭。
#[tauri::command]
pub fn ungroup_pin(app: AppHandle, label: String) -> Result<PinState, String> {
    if !settings::current_toggles(&app).pin_enhance {
        return Err(i18n::t("pin.error.disabled"));
    }
    window_for(&app, &label)?;
    with_entry(&label, |entry| entry.group = None)?;
    normalize_groups();
    schedule_persist(&app);
    broadcast_state(&app);
    state_for(&label)
}

/// 组内成员少于 2 的组按未分组处理,避免留下无法联动的孤立组 id。
fn normalize_groups() {
    with_store(|slots| {
        for index in 0..PIN_MAX {
            let Some(group) = slots[index].as_ref().and_then(|entry| entry.group.clone()) else {
                continue;
            };
            if group_size(slots, &Some(group)) > 1 {
                continue;
            }
            if let Some(entry) = slots[index].as_mut() {
                entry.group = None;
            }
        }
    });
}

fn group_targets(slots: &[Option<PinEntry>; PIN_MAX], slot: usize, enhanced: bool) -> Vec<usize> {
    let mut targets = vec![slot];
    if !enhanced {
        return targets;
    }
    let Some(entry) = slots[slot].as_ref() else {
        return targets;
    };
    if group_size(slots, &entry.group) <= 1 {
        return targets;
    }
    let group = entry.group.clone();
    for (index, member) in slots.iter().enumerate() {
        if index != slot && member.as_ref().is_some_and(|member| member.group == group) {
            targets.push(index);
        }
    }
    targets
}

/// 远离所有显示器且无重叠的位置视为陈旧回执(池窗口复用前停到 -32000
/// 的移动事件可能晚于新贴图入库),忽略以免把新贴图拖出屏幕。
fn position_is_stale(position: LogicalPoint, size: (f64, f64), areas: &[LogicalRect]) -> bool {
    if areas.is_empty() {
        return false;
    }
    const STALE_MARGIN: f64 = 600.0;
    areas.iter().all(|area| {
        overlap_area(position, size, *area) <= 0.0
            && rect_distance(position, size, *area) > STALE_MARGIN
    })
}

fn rect_distance(position: LogicalPoint, size: (f64, f64), area: LogicalRect) -> f64 {
    let left = position.0;
    let top = position.1;
    let right = position.0 + size.0.max(1.0);
    let bottom = position.1 + size.1.max(1.0);
    let dx = (area.0 - right).max(left - (area.0 + area.2)).max(0.0);
    let dy = (area.1 - bottom).max(top - (area.1 + area.3)).max(0.0);
    (dx * dx + dy * dy).sqrt()
}

/// 窗口移动回执:自身是增量起点,组内其它成员同步位移;
/// 后端移动成员窗口产生的回执增量约为 0,不会递归。
#[tauri::command]
pub fn move_pin(app: AppHandle, label: String, x: f64, y: f64) -> Result<PinState, String> {
    window_for(&app, &label)?;
    if !x.is_finite() || !y.is_finite() {
        return state_for(&label);
    }
    let advanced = settings::current_toggles(&app).pin_enhance;
    type MovePlan = (f64, f64, Vec<usize>, (f64, f64));
    let plan = with_store(|slots| -> Result<MovePlan, String> {
        let slot = slot_from_label(&label).ok_or_else(|| i18n::t("error.pin.window_unknown"))?;
        let entry = slots[slot]
            .as_ref()
            .ok_or_else(|| i18n::t("error.pin.source_gone"))?;
        Ok((
            x - entry.position.0,
            y - entry.position.1,
            group_targets(slots, slot, advanced),
            (entry.window_width, entry.window_height),
        ))
    })?;
    let (dx, dy, targets, size) = plan;
    if dx.abs() < MOVE_EPSILON && dy.abs() < MOVE_EPSILON {
        return state_for(&label);
    }
    // 仅对大幅度的回执做陈旧位置检查(停窗回执),正常拖动不枚举显示器。
    if dx.hypot(dy) > 1000.0 && position_is_stale((x, y), size, &monitor_areas(&app)) {
        return state_for(&label);
    }
    let mut positions = Vec::new();
    with_store(|slots| {
        for &slot in &targets {
            let Some(entry) = slots[slot].as_mut() else {
                continue;
            };
            entry.position.0 += dx;
            entry.position.1 += dy;
            positions.push((slot, entry.position));
        }
    });
    for (slot, position) in positions {
        if let Some(window) = app.get_webview_window(&slot_label(slot)) {
            let _ = window.set_position(LogicalPosition {
                x: position.0,
                y: position.1,
            });
        }
    }
    schedule_persist(&app);
    state_for(&label)
}

/// 滚轮缩放:倍率在 Rust 钳制,组内成员以同一锚点等比缩放。
#[tauri::command]
pub fn zoom_pin(
    app: AppHandle,
    label: String,
    zoom_in: bool,
    anchor_x: f64,
    anchor_y: f64,
) -> Result<PinState, String> {
    window_for(&app, &label)?;
    let current = with_entry(&label, |entry| entry.zoom())?;
    let target = if zoom_in {
        current * ZOOM_STEP
    } else {
        current / ZOOM_STEP
    };
    let target = target.clamp(MIN_ZOOM, MAX_ZOOM);
    apply_scale(
        &app,
        &label,
        target / current.max(f64::EPSILON),
        (anchor_x, anchor_y),
    )
}

#[tauri::command]
pub fn reset_pin_zoom(app: AppHandle, label: String) -> Result<PinState, String> {
    window_for(&app, &label)?;
    let current = with_entry(&label, |entry| entry.zoom())?;
    let anchor = with_entry(&label, |entry| {
        (
            entry.position.0 + entry.window_width / 2.0,
            entry.position.1 + entry.window_height / 2.0,
        )
    })?;
    apply_scale(&app, &label, 1.0 / current.max(f64::EPSILON), anchor)
}

fn apply_scale(
    app: &AppHandle,
    label: &str,
    factor: f64,
    anchor: LogicalPoint,
) -> Result<PinState, String> {
    if !factor.is_finite() || factor <= 0.0 {
        return state_for(label);
    }
    let advanced = settings::current_toggles(app).pin_enhance;
    let targets = with_store(|slots| -> Result<Vec<usize>, String> {
        let slot = slot_from_label(label).ok_or_else(|| i18n::t("error.pin.window_unknown"))?;
        if slots[slot].is_none() {
            return Err(i18n::t("error.pin.source_gone"));
        }
        Ok(group_targets(slots, slot, advanced))
    })?;
    let mut geometry = Vec::new();
    with_store(|slots| {
        for &slot in &targets {
            let Some(entry) = slots[slot].as_mut() else {
                continue;
            };
            let (position, size) = scaled_geometry(
                entry.position,
                (entry.window_width, entry.window_height),
                factor,
                anchor,
            );
            entry.position = position;
            entry.window_width = size.0;
            entry.window_height = size.1;
            geometry.push((slot, position, size));
        }
    });
    for (slot, position, size) in geometry {
        if let Some(window) = app.get_webview_window(&slot_label(slot)) {
            let _ = window.set_position(LogicalPosition {
                x: position.0,
                y: position.1,
            });
            let _ = window.set_size(LogicalSize {
                width: size.0,
                height: size.1,
            });
        }
    }
    schedule_persist(app);
    broadcast_state(app);
    state_for(label)
}

// ---------------------------------------------------------------------------
// 命令:复制、保存、再标注、关闭。
// ---------------------------------------------------------------------------

/// 复制当前显示内容(旋转/翻转/透明度已应用)为无损 PNG。
#[tauri::command]
pub async fn copy_pin(app: AppHandle, label: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        move || {
            let frame = transformed_frame_for(&app, &label)?;
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
pub async fn save_pin(app: AppHandle, label: String) -> Result<PinSaveResult, String> {
    let frame = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        let label = label.clone();
        move || transformed_frame_for(&app, &label)
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
    std::fs::write(&path, bytes).map_err(|error| {
        i18n::tp(
            "error.pin.save_to_path",
            &[
                ("path", &path.display().to_string()),
                ("error", &error.to_string()),
            ],
        )
    })?;
    Ok(PinSaveResult {
        saved: true,
        path: Some(path.to_string_lossy().into_owned()),
    })
}

/// 当前显示内容(旋转/翻转/透明度已应用);复制/保存/再标注共用。
fn transformed_frame_for(app: &AppHandle, label: &str) -> Result<Frame, String> {
    if app.get_webview_window(label).is_none() {
        return Err(i18n::t("error.pin.window_closed"));
    }
    let entry = entry_snapshot(label)?;
    Ok(transformed_frame(&entry))
}

/// 贴图再标注(R9):把当前显示内容装入预览会话,并标记回写目标 label;
/// 确认/取消由 `update_pin_from_preview` 与预览关闭路径收尾。打开期间来源
/// 贴图取消置顶,避免盖住编辑器。
#[tauri::command]
pub async fn begin_pin_edit(app: AppHandle, label: String) -> Result<(), String> {
    let frame = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        let label = label.clone();
        move || transformed_frame_for(&app, &label)
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
    let rendered =
        tauri::async_runtime::spawn_blocking(move || rasterize(&frame, &annotations).map_err(fail))
            .await
            .map_err(|_| i18n::t("error.pin.thread_pin"))??;

    let slot = slot_from_label(&label).ok_or_else(|| i18n::t("error.pin.window_unknown"))?;
    let stored = with_store(|slots| {
        let Some(entry) = slots[slot].as_mut() else {
            return Err(i18n::t("error.pin.closed"));
        };
        replace_source_content(entry, rendered);
        Ok(())
    });
    stored?;
    schedule_persist(&app);

    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.set_always_on_top(true);
        let _ = window.emit("pin-reload", "writeback");
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

/// 关闭目标:编组(成员 ≥ 2)时返回整组标签;否则只有自己。
fn close_targets(label: &str) -> Vec<String> {
    with_store(|slots| {
        let Some(slot) = slot_from_label(label) else {
            return Vec::new();
        };
        let Some(entry) = slots[slot].as_ref() else {
            return Vec::new();
        };
        if group_size(slots, &entry.group) <= 1 {
            return vec![label.to_string()];
        }
        let group = entry.group.clone();
        slots
            .iter()
            .enumerate()
            .filter(|(_, member)| member.as_ref().is_some_and(|member| member.group == group))
            .map(|(index, _)| slot_label(index))
            .collect()
    })
}

fn close_entries(app: &AppHandle, labels: &[String]) {
    let mut ids = Vec::new();
    for label in labels {
        let Some(slot) = slot_from_label(label) else {
            continue;
        };
        let removed = with_store(|slots| slots[slot].take());
        let Some(entry) = removed else {
            continue;
        };
        ids.push(entry.id);
        if let Some(window) = app.get_webview_window(label) {
            // 关闭前清掉穿透,避免池窗口带着空输入区被复用。
            let _ = window.set_ignore_cursor_events(false);
            let _ = window.emit("pin-cleared", ());
            park_pin_window(&window);
        }
    }
    delete_entries(app, &ids);
}

/// 关闭贴图:编组内任一成员关闭即关闭整组(菜单文案先提示);
/// 关闭即从存储删除,重启不重现。
#[tauri::command]
pub fn close_pin(app: AppHandle, label: String) {
    let targets = close_targets(&label);
    let targets = if targets.is_empty() {
        vec![label]
    } else {
        targets
    };
    close_entries(&app, &targets);
    crate::tray::refresh_menu(&app);
}

#[tauri::command]
pub fn close_all_pins(app: AppHandle) {
    let labels: Vec<String> = with_store(|slots| {
        (0..PIN_MAX)
            .filter(|&slot| slots[slot].is_some())
            .map(slot_label)
            .collect()
    });
    close_entries(&app, &labels);
    crate::tray::refresh_menu(&app);
}

/// 退出清理:先落盘(防抖窗口内的变化不丢),再关闭全部贴图窗口;
/// 与用户关闭不同,退出不删除存储,重启由 `pin_restore` 决定是否恢复。
/// 先置位退出闸门,避免清空内存后的后台防抖写入把空索引写回。
pub fn close_all(app: &AppHandle) {
    let already_exiting = EXITING.swap(true, Ordering::SeqCst);
    if !already_exiting {
        write_snapshot(app);
    }
    for slot in 0..PIN_MAX {
        with_store(|slots| slots[slot] = None);
        if let Some(window) = app.get_webview_window(&slot_label(slot)) {
            let _ = window.close();
        }
    }
}

/// 窗口销毁(手动关闭/显示器断开/应用退出)时清掉内存状态并从存储删除,
/// 保证用户关闭的贴图不会因下次恢复重现;退出路径先由 `close_all` 清空
/// 内存,再销毁窗口,因此正常退出不触发删除。
pub fn handle_destroyed(app: &AppHandle, label: &str) {
    let Some(slot) = slot_from_label(label) else {
        return;
    };
    let removed = with_store(|slots| slots[slot].take());
    let Some(entry) = removed else {
        return;
    };
    delete_entries(app, &[entry.id]);
    crate::tray::refresh_menu(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, rgba: Vec<u8>) -> Frame {
        Frame {
            width,
            height,
            rgba,
            scale: 1.0,
        }
    }

    fn entry(frame: Frame) -> PinEntry {
        PinEntry::new("p1".into(), frame, 40.0, 20.0, (10.0, 20.0))
    }

    fn slots_with(entries: Vec<(usize, PinEntry)>) -> [Option<PinEntry>; PIN_MAX] {
        let mut slots: [Option<PinEntry>; PIN_MAX] = Default::default();
        for (slot, entry) in entries {
            slots[slot] = Some(entry);
        }
        slots
    }

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
    fn generated_tokens_are_persistable() {
        let id = next_token('p');
        let group = next_token('g');
        assert!(id.starts_with('p'));
        assert!(group.starts_with('g'));
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(!id.is_empty() && id.len() <= 80);
        assert_ne!(id, next_token('p'));
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
        assert!(full.contains('8'));
    }

    #[test]
    fn quiet_pin_toast_keeps_other_readable_errors() {
        assert_eq!(
            quiet_pin_toast("无法创建贴图窗口：timeout"),
            "无法创建贴图窗口：timeout"
        );
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
        let frame = frame(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]);
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
    fn flip_frame_horizontal_mirrors_columns() {
        let frame = frame(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let flipped = flip_frame_horizontal(&frame);
        assert_eq!((flipped.width, flipped.height), (2, 1));
        assert_eq!(&flipped.rgba[0..4], &[5, 6, 7, 8]);
        assert_eq!(&flipped.rgba[4..8], &[1, 2, 3, 4]);
    }

    #[test]
    fn flip_frame_vertical_mirrors_rows() {
        let frame = frame(2, 2, (0..16).collect());
        let flipped = flip_frame_vertical(&frame);
        assert_eq!(&flipped.rgba[0..8], &(0..16).collect::<Vec<u8>>()[8..16]);
        assert_eq!(&flipped.rgba[8..16], &(0..16).collect::<Vec<u8>>()[0..8]);
    }

    #[test]
    fn flip_twice_returns_the_original_pixels() {
        let frame = frame(3, 2, (0..24).collect());
        let horizontal = flip_frame_horizontal(&flip_frame_horizontal(&frame));
        assert_eq!(horizontal.rgba, frame.rgba);
        let vertical = flip_frame_vertical(&flip_frame_vertical(&frame));
        assert_eq!(vertical.rgba, frame.rgba);
    }

    #[test]
    fn opacity_scales_alpha_without_touching_color() {
        let frame = frame(2, 1, vec![10, 20, 30, 200, 1, 2, 3, 255]);
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
    fn transformed_frame_applies_rotation_flip_then_opacity() {
        let mut source = entry(frame(2, 1, vec![255, 0, 0, 200, 0, 0, 255, 200]));
        source.scale = 1.5;
        source.logical_width = 40.0;
        source.logical_height = 20.0;
        source.rotation = 90;
        source.flip_h = true;
        source.opacity = 0.5;
        let frame = transformed_frame(&source);
        // 旋转后 1x2:上红下蓝;再水平翻转仍是单列,颜色顺序不变。
        assert_eq!((frame.width, frame.height), (1, 2));
        assert_eq!(frame.scale, 1.5);
        assert_eq!(&frame.rgba[0..3], &[255, 0, 0]);
        assert_eq!(frame.rgba[3], 100);
        assert_eq!(frame.rgba[7], 100);
        // 非法角度按整除归零,不越界 panic。
        let mut plain = source.clone();
        plain.rotation = 45;
        plain.flip_h = false;
        let plain = transformed_frame(&plain);
        assert_eq!((plain.width, plain.height), (2, 1));
    }

    #[test]
    fn transformed_frame_flip_matches_display_orientation() {
        // 1x2 上红下蓝,垂直翻转后上蓝下红:画面/复制/保存同源。
        let mut source = entry(frame(1, 2, vec![255, 0, 0, 255, 0, 0, 255, 255]));
        source.flip_v = true;
        let frame = transformed_frame(&source);
        assert_eq!(&frame.rgba[0..4], &[0, 0, 255, 255]);
        assert_eq!(&frame.rgba[4..8], &[255, 0, 0, 255]);
    }

    #[test]
    fn replace_source_content_resets_transform_and_logical_size() {
        let mut source = entry(frame(2, 1, vec![0; 8]));
        source.rotation = 90;
        source.flip_h = true;
        source.opacity = 0.25;
        let rendered = Frame {
            width: 1,
            height: 2,
            rgba: vec![1; 8],
            scale: 2.0,
        };
        replace_source_content(&mut source, rendered);
        assert_eq!((source.width, source.height), (1, 2));
        assert_eq!(source.rotation, 0);
        assert!(!source.flip_h && !source.flip_v);
        assert_eq!(source.opacity, 1.0);
        // 逻辑基准尺寸 = 渲染物理尺寸 / scale。
        assert!((source.logical_width - 0.5).abs() < 1e-9);
        assert!((source.logical_height - 1.0).abs() < 1e-9);
        // 内容变化必须触发 PNG 重写。
        assert_ne!(source.persisted_seq, source.content_seq);
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
        let mut source = entry(frame(1, 3, vec![9; 12]));
        source.scale = 2.0;
        let frame = source_frame(&source);
        assert_eq!((frame.width, frame.height), (1, 3));
        assert_eq!(frame.rgba, source.rgba);
        assert_eq!(frame.scale, 2.0);
    }

    #[test]
    fn entry_record_round_trips_geometry_and_transform() {
        let mut source = entry(frame(2, 2, vec![0; 16]));
        source.id = "p42-1".into();
        source.position = (100.0, 200.0);
        source.window_width = 300.0;
        source.window_height = 150.0;
        source.rotation = 270;
        source.flip_v = true;
        source.opacity = 0.75;
        source.group = Some("g1".into());
        let record = source.record();
        assert_eq!(record.id, "p42-1");
        assert_eq!((record.x, record.y), (100.0, 200.0));
        assert_eq!((record.width, record.height), (300.0, 150.0));
        assert_eq!(record.rotation, 270);
        assert!(record.flip_v && !record.flip_h);
        assert_eq!(record.opacity, 0.75);
        assert_eq!(record.group.as_deref(), Some("g1"));
    }

    #[test]
    fn rotated_geometry_keeps_the_center_and_swaps_size() {
        let (position, size) = rotated_geometry((100.0, 50.0), (400.0, 300.0));
        assert_eq!(size, (300.0, 400.0));
        // 中心不变:(100+200, 50+150) == (150+150, 100+200)。
        assert_eq!(
            (position.0 + size.0 / 2.0, position.1 + size.1 / 2.0),
            (300.0, 200.0)
        );
    }

    #[test]
    fn scaled_geometry_scales_around_the_anchor() {
        let (position, size) = scaled_geometry((100.0, 100.0), (200.0, 100.0), 2.0, (0.0, 0.0));
        assert_eq!(position, (200.0, 200.0));
        assert_eq!(size, (400.0, 200.0));
        // 锚点自身不动。
        let (position, _) = scaled_geometry((50.0, 60.0), (10.0, 10.0), 0.5, (50.0, 60.0));
        assert_eq!(position, (50.0, 60.0));
    }

    #[test]
    fn clamp_pin_position_keeps_pins_inside_the_visible_area() {
        let areas = [(0.0, 0.0, 1920.0, 1080.0)];
        // 已在可见区内:原样返回。
        assert_eq!(
            clamp_pin_position((100.0, 100.0), (400.0, 300.0), &areas),
            (100.0, 100.0)
        );
        // 越界:拉回显示器内。
        assert_eq!(
            clamp_pin_position((1800.0, 1000.0), (400.0, 300.0), &areas),
            (1920.0 - 400.0, 1080.0 - 300.0)
        );
        assert_eq!(
            clamp_pin_position((-500.0, -500.0), (400.0, 300.0), &areas),
            (0.0, 0.0)
        );
    }

    #[test]
    fn clamp_pin_position_prefers_the_monitor_with_overlap() {
        // 双屏:右屏在主屏右侧。已在右屏区域内 → 不动。
        let areas = [(0.0, 0.0, 1920.0, 1080.0), (1920.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(
            clamp_pin_position((2000.0, 100.0), (400.0, 300.0), &areas),
            (2000.0, 100.0)
        );
    }

    #[test]
    fn clamp_pin_position_moves_pins_back_from_a_removed_monitor() {
        // 记录来自已拔掉的第二块屏:无重叠时取中心最近的显示器。
        let areas = [(0.0, 0.0, 1920.0, 1080.0)];
        assert_eq!(
            clamp_pin_position((2600.0, 200.0), (400.0, 300.0), &areas),
            (1920.0 - 400.0, 200.0)
        );
    }

    #[test]
    fn clamp_pin_position_handles_window_larger_than_a_monitor() {
        let areas = [(100.0, 50.0, 300.0, 200.0)];
        assert_eq!(
            clamp_pin_position((500.0, 500.0), (400.0, 300.0), &areas),
            (100.0, 50.0)
        );
        // 无显示器信息:不移动。
        assert_eq!(
            clamp_pin_position((5.0, 6.0), (400.0, 300.0), &[]),
            (5.0, 6.0)
        );
    }

    #[test]
    fn display_change_signature_tracks_layout() {
        let first = [(0.0, 0.0, 1920.0, 1080.0)];
        let second = [(0.0, 0.0, 1920.0, 1080.0), (1920.0, 0.0, 1280.0, 720.0)];
        assert_eq!(monitor_signature(&first), monitor_signature(&first));
        assert_ne!(monitor_signature(&first), monitor_signature(&second));
    }

    #[test]
    fn stale_park_echoes_are_not_treated_as_real_moves() {
        let areas = [(0.0, 0.0, 1920.0, 1080.0)];
        // 停窗位置(-32000)是池窗口复用前的陈旧回执。
        assert!(position_is_stale(
            (-32000.0, -32000.0),
            (400.0, 300.0),
            &areas
        ));
        // 可见区内/接近屏幕边缘(用户主动拖出)不算陈旧。
        assert!(!position_is_stale((100.0, 100.0), (400.0, 300.0), &areas));
        assert!(!position_is_stale((1700.0, 900.0), (400.0, 300.0), &areas));
        assert!(!position_is_stale((2300.0, 100.0), (400.0, 300.0), &areas));
        // 无显示器信息时不判陈旧。
        assert!(!position_is_stale(
            (-32000.0, -32000.0),
            (400.0, 300.0),
            &[]
        ));
    }

    #[test]
    fn rotated_logical_size_follows_rotation_orientation() {
        assert_eq!(rotated_logical_size(800, 600, 2.0, 0), (400.0, 300.0));
        assert_eq!(rotated_logical_size(800, 600, 2.0, 90), (300.0, 400.0));
        assert_eq!(rotated_logical_size(800, 600, 2.0, 270), (300.0, 400.0));
    }

    #[test]
    fn sanitize_window_size_falls_back_to_logical_size() {
        assert_eq!(sanitize_window_size(0.0, 0.0, (40.0, 20.0)), (40.0, 20.0));
        assert_eq!(
            sanitize_window_size(-5.0, f64::NAN, (40.0, 20.0)),
            (40.0, 20.0)
        );
        assert_eq!(sanitize_window_size(80.0, 40.0, (40.0, 20.0)), (80.0, 40.0));
    }

    #[test]
    fn grouping_is_only_effective_with_two_members() {
        let mut first = entry(frame(1, 1, vec![0; 4]));
        first.group = Some("g1".into());
        let mut second = entry(frame(1, 1, vec![0; 4]));
        second.group = Some("g1".into());
        let slots = slots_with(vec![(0, first), (1, second)]);
        let state = state_from(&slots, "pin-1", slots[0].as_ref().unwrap());
        assert!(state.grouped);
        assert_eq!(state.group_size, 2);
        // 组内只剩自己:按未分组处理(避免孤立组 id 无法退出)。
        let mut loner = entry(frame(1, 1, vec![0; 4]));
        loner.group = Some("g9".into());
        let slots = slots_with(vec![(0, loner)]);
        let state = state_from(&slots, "pin-1", slots[0].as_ref().unwrap());
        assert!(!state.grouped);
        assert_eq!(state.group_size, 1);
    }

    #[test]
    fn group_targets_link_members_only_when_enhanced() {
        let mut first = entry(frame(1, 1, vec![0; 4]));
        first.group = Some("g1".into());
        let mut second = entry(frame(1, 1, vec![0; 4]));
        second.group = Some("g1".into());
        let slots = slots_with(vec![(0, first), (2, second)]);
        assert_eq!(group_targets(&slots, 0, true), vec![0, 2]);
        // 功能开关关闭时只移动自己,编组联动随开关消失。
        assert_eq!(group_targets(&slots, 0, false), vec![0]);
        // 未编组:只移动自己。
        let mut solo = entry(frame(1, 1, vec![0; 4]));
        solo.group = None;
        let slots = slots_with(vec![(0, solo)]);
        assert_eq!(group_targets(&slots, 0, true), vec![0]);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn click_through_is_supported_on_windows_and_macos() {
        assert!(click_through_supported());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wayland_sessions_are_detected_for_click_through() {
        assert!(wayland_session(None, true));
        assert!(wayland_session(Some("wayland"), false));
        assert!(wayland_session(Some("Wayland"), false));
        assert!(!wayland_session(Some("x11"), false));
        assert!(!wayland_session(None, false));
    }
}
