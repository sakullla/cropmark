use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use crate::annotate::{rasterize, Annotation};
use crate::capture::buffer::encode_png;
use crate::capture::error::CaptureError;
use crate::capture::session;

/// 同时存在的贴图上限:标签 pin-1..pin-N 轮转复用空闲槽位。
pub const PIN_MAX: usize = 8;
pub const PIN_LABEL_PREFIX: &str = "pin-";

/// 轮转游标:下一次分配从上一次分配槽位之后开始找空闲标签。
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// 新建贴图的 PNG 交接邮箱:窗口创建后由前端 `get_pin_image` 取走即清空,
/// Rust 侧不长期持有图像副本(取走/窗口销毁都清槽)。
static MAILBOX: Mutex<[Option<Vec<u8>>; PIN_MAX]> =
    Mutex::new([None, None, None, None, None, None, None, None]);

fn with_mailbox<R>(f: impl FnOnce(&mut [Option<Vec<u8>>; PIN_MAX]) -> R) -> R {
    let mut guard = MAILBOX
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
        "无法完成贴图。".into()
    } else {
        message
    }
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

/// 打开一张贴图:轮转分配空闲标签,建置顶无边框、skip_taskbar、不可调
/// 尺寸的窗口,初始逻辑尺寸为图像逻辑大小,位置在光标附近(钳制在
/// 工作区内)。PNG 经邮箱交给前端,Rust 不留长期副本。
pub fn open_pin(
    app: &AppHandle,
    png: Vec<u8>,
    logical_width: f64,
    logical_height: f64,
) -> Result<WebviewWindow, String> {
    let cursor = NEXT_SLOT.load(Ordering::SeqCst);
    // 槽位占用以真实窗口存在性为准:窗口销毁(Destroyed)即视为空闲。
    let slot = pick_slot(cursor, |slot| {
        app.get_webview_window(&slot_label(slot)).is_some()
    })
    .ok_or_else(|| "贴图最多同时 8 张，请先关闭部分贴图。".to_string())?;
    NEXT_SLOT.store((slot + 1) % PIN_MAX, Ordering::SeqCst);

    let label = slot_label(slot);
    let (cursor_pos, work) = pointer_work_area(app);
    let (width, height) = (logical_width.max(1.0), logical_height.max(1.0));
    let (x, y) = pin_origin(cursor_pos, work, width, height);

    with_mailbox(|slots| slots[slot] = Some(png));
    let window = WebviewWindowBuilder::new(
        app,
        label.clone(),
        WebviewUrl::App("index.html?view=pin".into()),
    )
    .title("Cropmark")
    .decorations(false)
    .shadow(true)
    .skip_taskbar(true)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .always_on_top(true)
    .visible(false)
    .inner_size(width, height)
    .build()
    .map_err(|error| {
        with_mailbox(|slots| slots[slot] = None);
        format!("无法创建贴图窗口：{error}")
    })?;
    // 先定位再显示,避免窗口在左上角闪现后再跳到光标附近。
    let _ = window.set_position(tauri::Position::Logical(tauri::LogicalPosition { x, y }));
    let _ = window.set_size(tauri::Size::Logical(tauri::LogicalSize { width, height }));
    let _ = window.set_ignore_cursor_events(false);
    let _ = window.set_always_on_top(true);
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();
    Ok(window)
}

struct PreparedPin {
    png: Vec<u8>,
    width: f64,
    height: f64,
}

/// 从会话保留帧(预览帧或 Quiet TTL 帧)合成 PNG 与窗口尺寸;不含建窗。
fn prepare_pin(app: &AppHandle, annotations: &[Annotation]) -> Result<PreparedPin, String> {
    let frame = session::current_preview_frame(app).map_err(fail)?;
    let rendered = rasterize(&frame, annotations).map_err(fail)?;
    let png = encode_png(&rendered).map_err(fail)?;
    let (_, work) = pointer_work_area(app);
    let (work_w, work_h) = work.map(|(.., w, h)| (w, h)).unwrap_or((1920.0, 1080.0));
    let (width, height) = pin_logical_size(frame.width, frame.height, frame.scale, work_w, work_h);
    Ok(PreparedPin { png, width, height })
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
    .map_err(|_| "贴图线程失败。".to_string())??;
    open_pin(&app, prepared.png, prepared.width, prepared.height).map(|_| ())
}

/// Quiet Pin 动作入口(选区操作条 Pin:不经前端、不带标注)。
/// 供 capture 动作分发接线(capture/mod.rs `run_quiet_action` 的 Pin 分支,
/// 归 shell-wiring 任务):成功无提示(贴图窗即反馈),失败走 toast。
#[allow(dead_code)]
pub fn pin_retained(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if pin_current(app.clone(), Vec::new()).await.is_err() {
            crate::capture::ui::show_toast(&app, "贴图失败，请重试。");
        }
    });
}

/// 前端拉取本窗口的 PNG(取走即清,内存只留在前端)。
#[tauri::command]
pub fn get_pin_image(app: AppHandle, label: String) -> Result<tauri::ipc::Response, String> {
    let slot = slot_from_label(&label).ok_or_else(|| "未知贴图窗口。".to_string())?;
    if app.get_webview_window(&label).is_none() {
        return Err("贴图窗口已关闭。".to_string());
    }
    let png = with_mailbox(|slots| slots[slot].take())
        .ok_or_else(|| "贴图图像已失效，请重新贴图。".to_string())?;
    Ok(tauri::ipc::Response::new(png))
}

#[tauri::command]
pub fn close_pin(app: AppHandle, label: String) {
    if let Some(slot) = slot_from_label(&label) {
        with_mailbox(|slots| slots[slot] = None);
    }
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.close();
    }
}

#[tauri::command]
pub fn close_all_pins(app: AppHandle) {
    close_all(&app);
}

/// 退出清理:遍历关闭全部贴图窗口(标签占用随窗口销毁自动释放)。
pub fn close_all(app: &AppHandle) {
    for slot in 0..PIN_MAX {
        with_mailbox(|slots| slots[slot] = None);
        if let Some(window) = app.get_webview_window(&slot_label(slot)) {
            let _ = window.close();
        }
    }
}

/// 窗口销毁(手动关闭/显示器断开/应用退出)时清掉交接邮箱,
/// 防止未取走的 PNG 滞留 Rust 侧。
pub fn handle_destroyed(label: &str) {
    if let Some(slot) = slot_from_label(label) {
        with_mailbox(|slots| slots[slot] = None);
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
}
