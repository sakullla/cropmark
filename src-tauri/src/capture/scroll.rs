//! R1 手动滚动长截图(ADR-5)。
//!
//! 选区壳确认固定物理区域后进入独立滚动会话:后端按约 300ms 周期抓取该
//! 区域,以「上一帧底部条带在当前帧中的垂直位置」估算滚动位移并垂直拼接;
//! Web 控制窗(`index.html?view=scroll`,置顶非模态、位于选区旁)承载状态
//! 提示与「完成/取消」。只支持垂直滚动:内容未变化连续若干次、匹配失败、
//! 滚动过快都给出可理解的状态提示并允许继续或取消;拼接高度设上限,达到
//! 上限自动完成并提示。
//!
//! 完成结果走 `session::finish_scroll_frame`(现有预览完成路径,历史与
//! 「上次区域」规则同源);取消、内容未变化、平台不支持三类路径给出提示且
//! 不写剪贴板、历史或磁盘。

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalPosition, Manager, Position, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};

use super::buffer::{crop_rgba, Frame};
use super::error::CaptureError;
use super::geometry::MonitorGeom;
use super::platform;
use super::session::{self, RegionSelection};
use crate::annotate::Annotation;

/// 控制窗标签:滚动会话期间唯一,结束后隐藏复用(toast/error 同模式)。
pub const WINDOW: &str = "scroll";

/// 周期抓取间隔(ADR-5:约 300ms)。
pub(crate) const CAPTURE_INTERVAL: Duration = Duration::from_millis(300);

/// 底部匹配条带的最大高度(区域更矮时取 1/3,保证候选位移搜索范围)。
pub(crate) const STRIP_HEIGHT: u32 = 64;
/// 条带最多占区域高度的比例分母。
const STRIP_DIVISOR: u32 = 3;
/// 匹配采样步长(横向/纵向):屏幕内容的逐像素精确匹配过慢,采样足够稳定。
const SAMPLE_STEP_X: u32 = 3;
const SAMPLE_STEP_Y: u32 = 2;
/// 平均亮度差容忍上限(0–255);超过视为匹配失败。
const MATCH_MAX_MEAN_DIFF: u64 = 12;
/// 内容未变化连续达到该次数后给出提示(提示不终止会话)。
pub(crate) const UNCHANGED_HINT_AFTER: u32 = 3;
/// 拼接高度上限(物理像素):达到后自动完成并提示。
pub(crate) const MAX_STITCH_HEIGHT: u32 = 12_000;
/// 拼接像素总量上限:限制超宽区域的内存占用(高度上限随之收紧)。
pub(crate) const MAX_STITCH_PIXELS: u64 = 40_000_000;

/// 控制窗逻辑尺寸。
const CONTROL_WIDTH: f64 = 320.0;
const CONTROL_HEIGHT: f64 = 150.0;
/// 控制窗与选区之间的间距(逻辑像素)。
const CONTROL_GAP: f64 = 10.0;

// 滚动会话状态机(全局单会话;命令线程只投递请求,会话线程执行清理)。
const STATE_IDLE: u8 = 0;
const STATE_RUNNING: u8 = 1;
const STATE_FINISH: u8 = 2;
const STATE_FINISHING: u8 = 3;
const STATE_CANCEL: u8 = 4;

static SCROLL_STATE: AtomicU8 = AtomicU8::new(STATE_IDLE);
static SCROLL_GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_STATUS: Mutex<Option<ScrollStatus>> = Mutex::new(None);

/// 控制窗状态载荷:前端按 `state` 映射词条,`height` 为当前拼接高度。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollStatus {
    pub state: String,
    pub width: u32,
    pub height: u32,
    /// 已追加的行数(不含初始区域)。
    pub appended: u32,
}

impl ScrollStatus {
    fn new(state: &str, width: u32, height: u32, appended: u32) -> Self {
        Self {
            state: state.to_string(),
            width,
            height,
            appended,
        }
    }
}

/// 一次周期抓取的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollTick {
    /// 内容未变化;`hint` 表示连续未变化已达提示阈值。
    Unchanged { hint: bool },
    /// 已追加新内容;`fast` 表示位移超过区域高度一半(滚动过快提示)。
    Appended { fast: bool },
    /// 条带在当前帧中找不到可靠匹配:给出提示并允许继续/取消。
    NoMatch,
    /// 已达到拼接上限(已追加剩余部分),调用方应自动完成并提示。
    LimitReached,
}

/// 条带高度:不超过 `STRIP_HEIGHT` 且不超过区域高度的 1/3。
pub(crate) fn strip_height(frame_height: u32) -> u32 {
    (frame_height / STRIP_DIVISOR).clamp(1, STRIP_HEIGHT)
}

/// 按宽度收紧后的拼接高度上限。
pub(crate) fn stitch_cap_height(width: u32) -> u32 {
    let width = u64::from(width.max(1));
    let by_pixels = (MAX_STITCH_PIXELS / width).min(u64::from(MAX_STITCH_HEIGHT));
    (by_pixels as u32).max(1)
}

fn luma(px: &[u8]) -> u32 {
    (u32::from(px[0]) * 299 + u32::from(px[1]) * 587 + u32::from(px[2]) * 114) / 1000
}

/// 在 `next` 中寻找 `prev` 底部条带的垂直位置(只允许向上/不动,即只支持
/// 向下滚动内容):返回条带顶边 y(0..=prev.height-strip);平均亮度差超过
/// 容忍上限时返回 None。
pub(crate) fn match_strip_offset(prev: &Frame, next: &Frame) -> Option<u32> {
    if prev.width == 0 || prev.height < 2 || next.width != prev.width || next.height != prev.height
    {
        return None;
    }
    let stride = prev.width as usize * 4;
    let strip_h = strip_height(prev.height);
    let strip_y = prev.height - strip_h;
    let mut best_y = 0u32;
    let mut best_mean = u64::MAX;
    let mut candidate = 0u32;
    while candidate <= strip_y {
        let mut sum = 0u64;
        let mut count = 0u64;
        let mut sy = 0u32;
        while sy < strip_h {
            let prow = (strip_y + sy) as usize * stride;
            let nrow = (candidate + sy) as usize * stride;
            let mut sx = 0u32;
            while sx < prev.width {
                let p = prow + sx as usize * 4;
                let n = nrow + sx as usize * 4;
                let diff = luma(&prev.rgba[p..p + 4]).abs_diff(luma(&next.rgba[n..n + 4]));
                sum += u64::from(diff);
                count += 1;
                sx += SAMPLE_STEP_X;
            }
            sy += SAMPLE_STEP_Y;
        }
        let mean = sum / count.max(1);
        if mean < best_mean {
            best_mean = mean;
            best_y = candidate;
            if mean == 0 {
                break;
            }
        }
        candidate += 1;
    }
    if best_mean <= MATCH_MAX_MEAN_DIFF {
        Some(best_y)
    } else {
        None
    }
}

/// 垂直拼接器:持有初始区域与最近一帧,按位移追加新内容。
pub(crate) struct Stitcher {
    width: u32,
    scale: f64,
    rgba: Vec<u8>,
    height: u32,
    prev: Frame,
    cap_height: u32,
    appended: u32,
    unchanged_ticks: u32,
    scrolled: bool,
    limit_reached: bool,
}

impl Stitcher {
    pub(crate) fn new(initial: Frame, cap_height: u32) -> Self {
        let width = initial.width;
        let scale = initial.scale;
        let height = initial.height;
        let rgba = initial.rgba.clone();
        Self {
            width,
            scale,
            rgba,
            height,
            prev: initial,
            cap_height: cap_height.max(1),
            appended: 0,
            unchanged_ticks: 0,
            scrolled: false,
            limit_reached: false,
        }
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn appended(&self) -> u32 {
        self.appended
    }

    /// 是否产生过滚动内容;为假时完成按钮按「内容未变化」路径处理(无输出)。
    pub(crate) fn scrolled(&self) -> bool {
        self.scrolled
    }

    pub(crate) fn limit_reached(&self) -> bool {
        self.limit_reached
    }

    /// 处理一帧新抓取的区域画面。
    pub(crate) fn tick(&mut self, next: Frame) -> ScrollTick {
        if self.height >= self.cap_height {
            self.limit_reached = true;
            return ScrollTick::LimitReached;
        }
        let prev_height = self.prev.height;
        let strip_y = prev_height - strip_height(prev_height);
        let Some(y) = match_strip_offset(&self.prev, &next) else {
            // 匹配失败时用最新帧作为下一次的基准,避免持续对比已经消失的条带。
            self.prev = next;
            return ScrollTick::NoMatch;
        };
        let delta = strip_y.saturating_sub(y);
        if delta == 0 {
            self.unchanged_ticks += 1;
            self.prev = next;
            return ScrollTick::Unchanged {
                hint: self.unchanged_ticks >= UNCHANGED_HINT_AFTER,
            };
        }
        self.unchanged_ticks = 0;
        self.scrolled = true;
        let remaining = self.cap_height.saturating_sub(self.height);
        let append = delta.min(remaining);
        self.append_rows(&next, append);
        let fast = delta > prev_height / 2;
        self.prev = next;
        if append < delta || self.height >= self.cap_height {
            self.limit_reached = true;
            return ScrollTick::LimitReached;
        }
        ScrollTick::Appended { fast }
    }

    /// 追加 `next` 的末尾 `rows` 行(滚动 `rows` 像素后新增的内容)。
    fn append_rows(&mut self, next: &Frame, rows: u32) {
        if rows == 0 {
            return;
        }
        let stride = self.width as usize * 4;
        let start = (next.height - rows) as usize * stride;
        self.rgba
            .extend_from_slice(&next.rgba[start..start + rows as usize * stride]);
        self.height += rows;
        self.appended += rows;
    }

    pub(crate) fn into_frame(self) -> Frame {
        Frame {
            width: self.width,
            height: self.height,
            rgba: self.rgba,
            scale: self.scale,
        }
    }
}

/// 控制窗位置:优先贴选区右侧,右侧放不下改左侧,再往下/上;都放不下时
/// 收到显示器逻辑范围右下角(仍尽量不覆盖选区中心)。
pub(crate) fn control_origin(
    monitor: &MonitorGeom,
    region: &RegionSelection,
    window: (f64, f64),
) -> (f64, f64) {
    let scale = if monitor.scale.is_finite() && monitor.scale > 0.0 {
        monitor.scale
    } else {
        1.0
    };
    let mx = monitor.logical_x as f64;
    let my = monitor.logical_y as f64;
    let mw = monitor.logical_width as f64;
    let mh = monitor.logical_height as f64;
    let rx = mx + region.x as f64 / scale;
    let ry = my + region.y as f64 / scale;
    let rw = region.width as f64 / scale;
    let rh = region.height as f64 / scale;
    let (ww, wh) = window;
    let candidates = [
        (rx + rw + CONTROL_GAP, ry),
        (rx - ww - CONTROL_GAP, ry),
        (rx, ry + rh + CONTROL_GAP),
        (rx, ry - wh - CONTROL_GAP),
    ];
    for (x, y) in candidates {
        if x >= mx && y >= my && x + ww <= mx + mw && y + wh <= my + mh {
            return (x, y);
        }
    }
    let x = (mx + mw - ww - CONTROL_GAP).max(mx);
    let y = (my + mh - wh - CONTROL_GAP).max(my);
    (x, y)
}

fn emit_status(app: &AppHandle, status: ScrollStatus) {
    *LAST_STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(status.clone());
    let _ = app.emit_to(WINDOW, "scroll-status", status);
}

fn clear_status() {
    *LAST_STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// R1:开始一次滚动会话。冻结帧与显示器几何从会话槽位读取;平台不支持时
/// 返回明确失败文案(不进入选区、不产出)。
pub(crate) fn start(
    app: &AppHandle,
    generation: u64,
    region: RegionSelection,
    annotations: Vec<Annotation>,
) -> Result<(), CaptureError> {
    if !platform::scroll_capture_supported() {
        return Err(CaptureError::unavailable(
            "error.capture.scroll_unsupported",
        ));
    }
    // 同一时间只允许一个滚动会话:旧的按取消语义收尾(不产出)。
    interrupt(app);
    if SCROLL_STATE
        .compare_exchange(
            STATE_IDLE,
            STATE_RUNNING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return Err(CaptureError::api("error.capture.scroll_busy"));
    }
    SCROLL_GENERATION.store(generation, Ordering::SeqCst);
    let (freeze, monitor) = match session::scroll_source(app, generation) {
        Ok(source) => source,
        Err(error) => {
            release_state(generation);
            return Err(error);
        }
    };
    let initial = match crop_rgba(&freeze, region.x, region.y, region.width, region.height) {
        Ok(frame) => frame,
        Err(error) => {
            release_state(generation);
            return Err(error);
        }
    };
    let cap_height = stitch_cap_height(initial.width);
    if let Err(error) = open_control_window(app, &monitor, &region) {
        release_state(generation);
        dismiss_control_window_now(app);
        return Err(error);
    }
    emit_status(
        app,
        ScrollStatus::new("running", initial.width, initial.height, 0),
    );
    let handle = app.clone();
    let spawn = std::thread::Builder::new()
        .name("cropmark-scroll".into())
        .spawn(move || {
            run_session(
                handle,
                monitor,
                region,
                annotations,
                initial,
                cap_height,
                generation,
            );
        });
    if spawn.is_err() {
        release_state(generation);
        dismiss_control_window_now(app);
        return Err(CaptureError::api("error.capture.thread_failed"));
    }
    Ok(())
}

fn open_control_window(
    app: &AppHandle,
    monitor: &MonitorGeom,
    region: &RegionSelection,
) -> Result<WebviewWindow, CaptureError> {
    let (x, y) = control_origin(monitor, region, (CONTROL_WIDTH, CONTROL_HEIGHT));
    // 复用已存在的控制窗(toast/error 同模式):关闭再同标签重建存在
    // webview 销毁竞态;隐藏复用只重置显示状态并通知前端补拉最新状态。
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.set_position(Position::Logical(LogicalPosition { x, y }));
        let _ = window.set_always_on_top(true);
        let _ = window.set_ignore_cursor_events(false);
        let _ = window.show();
        let _ = window.emit("scroll-reload", ());
        return Ok(window);
    }
    let window = WebviewWindowBuilder::new(
        app,
        WINDOW,
        WebviewUrl::App("index.html?view=scroll".into()),
    )
    .title("Cropmark")
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .skip_taskbar(true)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .closable(true)
    .always_on_top(true)
    .focused(false)
    .inner_size(CONTROL_WIDTH, CONTROL_HEIGHT)
    .position(x, y)
    .build()
    .map_err(|error| CaptureError::api_detail("error.capture.window_build", &error.to_string()))?;
    let _ = window.set_ignore_cursor_events(false);
    Ok(window)
}

/// 收起控制窗:仅隐藏,保留预创建 webview 供下一次会话复用。
fn dismiss_control_window_now(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.set_always_on_top(false);
        let _ = window.hide();
    }
    clear_status();
}

/// 释放状态:仅当全局代际仍属于本会话时才回到 IDLE,避免收尾线程把已经
/// 开始的新滚动会话状态清掉。
fn release_state(generation: u64) {
    if SCROLL_GENERATION.load(Ordering::SeqCst) == generation {
        SCROLL_STATE.store(STATE_IDLE, Ordering::SeqCst);
    }
}

/// 收起本会话的控制窗(代际不再匹配说明已被新会话接管/已由取消路径收起)。
fn dismiss_control_window(app: &AppHandle, generation: u64) {
    if SCROLL_GENERATION.load(Ordering::SeqCst) == generation {
        dismiss_control_window_now(app);
    }
}

/// 新截取开始或应用退出:取消进行中的滚动会话(无输出);无会话时为 no-op。
pub(crate) fn interrupt(app: &AppHandle) {
    let state = SCROLL_STATE.load(Ordering::SeqCst);
    if !matches!(state, STATE_RUNNING | STATE_FINISH | STATE_FINISHING) {
        return;
    }
    SCROLL_STATE.store(STATE_CANCEL, Ordering::SeqCst);
    let generation = SCROLL_GENERATION.load(Ordering::SeqCst);
    dismiss_control_window_now(app);
    session::cancel_scroll_session(app, generation);
    SCROLL_STATE.store(STATE_IDLE, Ordering::SeqCst);
}

/// 控制窗被外部销毁(Alt+F4 等):按取消处理,不产出。
pub(crate) fn handle_window_destroyed(app: &AppHandle) {
    if SCROLL_STATE.load(Ordering::SeqCst) == STATE_RUNNING {
        interrupt(app);
    }
}

/// 应用退出时清理会话状态与控制窗。
pub(crate) fn shutdown(app: &AppHandle) {
    interrupt(app);
    dismiss_control_window_now(app);
}

fn run_session(
    app: AppHandle,
    monitor: MonitorGeom,
    region: RegionSelection,
    annotations: Vec<Annotation>,
    initial: Frame,
    cap_height: u32,
    generation: u64,
) {
    let mut stitcher = Stitcher::new(initial, cap_height);
    loop {
        match SCROLL_STATE.load(Ordering::SeqCst) {
            STATE_FINISH => break,
            STATE_CANCEL | STATE_IDLE => {
                dismiss_control_window(&app, generation);
                release_state(generation);
                return;
            }
            _ => {}
        }
        if !session::scroll_session_alive(&app, generation) {
            // 会话被替换/取消:不产出,只收尾窗口与状态。
            dismiss_control_window(&app, generation);
            release_state(generation);
            return;
        }
        std::thread::sleep(CAPTURE_INTERVAL);
        if SCROLL_STATE.load(Ordering::SeqCst) != STATE_RUNNING {
            continue;
        }
        let full = match platform::capture_monitor(&monitor) {
            Ok(frame) => frame,
            Err(_) => {
                emit_status(
                    &app,
                    ScrollStatus::new(
                        "failed",
                        region.width,
                        stitcher.height(),
                        stitcher.appended(),
                    ),
                );
                continue;
            }
        };
        let next = match crop_rgba(&full, region.x, region.y, region.width, region.height) {
            Ok(frame) => frame,
            Err(_) => {
                emit_status(
                    &app,
                    ScrollStatus::new(
                        "failed",
                        region.width,
                        stitcher.height(),
                        stitcher.appended(),
                    ),
                );
                continue;
            }
        };
        match stitcher.tick(next) {
            ScrollTick::Unchanged { hint } => emit_status(
                &app,
                ScrollStatus::new(
                    if hint { "unchanged" } else { "running" },
                    region.width,
                    stitcher.height(),
                    stitcher.appended(),
                ),
            ),
            ScrollTick::Appended { fast } => emit_status(
                &app,
                ScrollStatus::new(
                    if fast { "fast" } else { "running" },
                    region.width,
                    stitcher.height(),
                    stitcher.appended(),
                ),
            ),
            ScrollTick::NoMatch => emit_status(
                &app,
                ScrollStatus::new(
                    "no_match",
                    region.width,
                    stitcher.height(),
                    stitcher.appended(),
                ),
            ),
            ScrollTick::LimitReached => {
                emit_status(
                    &app,
                    ScrollStatus::new(
                        "limit",
                        region.width,
                        stitcher.height(),
                        stitcher.appended(),
                    ),
                );
                break;
            }
        }
    }
    SCROLL_STATE.store(STATE_FINISHING, Ordering::SeqCst);
    emit_status(
        &app,
        ScrollStatus::new(
            "finishing",
            stitcher.width(),
            stitcher.height(),
            stitcher.appended(),
        ),
    );
    finish_session(&app, stitcher, &annotations, &region, generation);
}

/// 完成会话:先置回 IDLE 再收起控制窗(窗口销毁事件在 IDLE 状态下不触发
/// 取消),未产生滚动内容时按「内容未变化」路径提示且不产出;达到上限时
/// 附加提示。结果走现有预览完成路径。
fn finish_session(
    app: &AppHandle,
    stitcher: Stitcher,
    annotations: &[Annotation],
    region: &RegionSelection,
    generation: u64,
) {
    let scrolled = stitcher.scrolled();
    let limit = stitcher.limit_reached();
    let frame = stitcher.into_frame();
    release_state(generation);
    dismiss_control_window(app, generation);
    if !scrolled {
        session::cancel_scroll_session(app, generation);
        super::ui::show_toast_key(app, "toast.scroll_no_change");
        return;
    }
    match session::finish_scroll_frame(app, frame, annotations.to_vec(), region, generation) {
        Ok(()) => {
            if limit {
                super::ui::show_toast_key(app, "toast.scroll_limit");
            }
        }
        Err(error) if !error.is_cancelled() => {
            super::ui::show_toast(app, &error.user_message());
        }
        Err(_) => {}
    }
}

/// 控制窗前端消息确认:请求完成(合成期间界面会显示进行中)。
#[tauri::command]
pub fn finish_scroll_capture(app: AppHandle) -> Result<(), CaptureError> {
    match SCROLL_STATE.compare_exchange(
        STATE_RUNNING,
        STATE_FINISH,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => {
            let last = get_scroll_status().unwrap_or_else(|| ScrollStatus::new("running", 0, 0, 0));
            emit_status(
                &app,
                ScrollStatus {
                    state: "finishing".into(),
                    ..last
                },
            );
            Ok(())
        }
        Err(STATE_FINISH) | Err(STATE_FINISHING) => Ok(()),
        Err(_) => Err(CaptureError::api("error.capture.scroll_missing")),
    }
}

/// 控制窗前端消息:取消滚动会话,不写剪贴板、历史或磁盘。
#[tauri::command]
pub fn cancel_scroll_capture(app: AppHandle) {
    interrupt(&app);
}

/// 控制窗晚于事件挂载时补拉当前状态。
#[tauri::command]
pub fn get_scroll_status() -> Option<ScrollStatus> {
    LAST_STATUS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行主频图案:相邻行亮度差远大于容忍上限,保证垂直匹配唯一。
    fn page(width: u32, height: u32, seed: u32) -> Frame {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height {
            let value = ((y * 53 + seed) % 251) as u8;
            for x in 0..width {
                let i = (y as usize * width as usize + x as usize) * 4;
                let jitter = (x % 3) as u8;
                rgba[i] = value.saturating_add(jitter);
                rgba[i + 1] = value;
                rgba[i + 2] = value.wrapping_sub(jitter);
                rgba[i + 3] = 255;
            }
        }
        Frame {
            width,
            height,
            rgba,
            scale: 1.0,
        }
    }

    /// 1:1 像素切片(测试中直接构造窗口画面)。
    fn viewport(page: &Frame, top: u32, height: u32) -> Frame {
        crop_rgba(page, 0, top, page.width, height).unwrap()
    }

    /// 与行纹图案无任何共同条带的画面(纯色)。
    fn flat(width: u32, height: u32, value: u8) -> Frame {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for px in rgba.chunks_exact_mut(4) {
            px[0] = value;
            px[1] = value;
            px[2] = value;
            px[3] = 255;
        }
        Frame {
            width,
            height,
            rgba,
            scale: 1.0,
        }
    }

    #[test]
    fn bottom_strip_matching_estimates_scroll_delta() {
        let page = page(96, 240, 0);
        let prev = viewport(&page, 0, 120);
        for delta in [1u32, 8, 37, 56] {
            let next = viewport(&page, delta, 120);
            let y = match_strip_offset(&prev, &next)
                .unwrap_or_else(|| panic!("delta {delta} must match"));
            let strip_y = 120 - strip_height(120);
            assert_eq!(strip_y - y, delta, "delta {delta}");
        }
    }

    #[test]
    fn unchanged_viewport_matches_at_rest_with_zero_delta() {
        let page = page(64, 160, 7);
        let prev = viewport(&page, 0, 120);
        let y = match_strip_offset(&prev, &prev.clone()).expect("identical frames match");
        assert_eq!(y, 120 - strip_height(120));
    }

    #[test]
    fn matching_fails_when_content_has_no_shared_band() {
        let prev = page(64, 120, 0);
        let next = flat(64, 120, 120);
        assert_eq!(match_strip_offset(&prev, &next), None);
    }

    #[test]
    fn stitcher_appends_exactly_the_new_rows_after_a_scroll() {
        let page = page(80, 300, 3);
        let initial = viewport(&page, 0, 120);
        let mut stitcher = Stitcher::new(initial, stitch_cap_height(80));
        let delta = 45;
        let tick = stitcher.tick(viewport(&page, delta, 120));
        assert_eq!(tick, ScrollTick::Appended { fast: false });
        assert_eq!(stitcher.height(), 120 + delta);
        assert_eq!(stitcher.appended(), delta);
        assert!(stitcher.scrolled());

        let stitched = stitcher.into_frame();
        let expected = crop_rgba(&page, 0, 0, 80, 120 + delta).unwrap();
        assert_eq!(stitched.width, expected.width);
        assert_eq!(stitched.height, expected.height);
        assert_eq!(stitched.rgba, expected.rgba);
    }

    #[test]
    fn stitcher_joins_multiple_scroll_steps_without_gaps() {
        let page = page(72, 400, 11);
        let mut stitcher = Stitcher::new(viewport(&page, 0, 100), 1000);
        let mut top = 0u32;
        for delta in [20u32, 7, 33, 12] {
            top += delta;
            assert_eq!(
                stitcher.tick(viewport(&page, top, 100)),
                ScrollTick::Appended { fast: false }
            );
        }
        let stitched = stitcher.into_frame();
        let expected = crop_rgba(&page, 0, 0, 72, 100 + top).unwrap();
        assert_eq!(stitched.rgba, expected.rgba);
    }

    #[test]
    fn unchanged_ticks_report_no_motion_and_hint_after_threshold() {
        let page = page(64, 200, 5);
        let initial = viewport(&page, 0, 100);
        let mut stitcher = Stitcher::new(initial.clone(), 1000);
        assert_eq!(
            stitcher.tick(initial.clone()),
            ScrollTick::Unchanged { hint: false }
        );
        assert_eq!(
            stitcher.tick(initial.clone()),
            ScrollTick::Unchanged { hint: false }
        );
        assert_eq!(stitcher.tick(initial), ScrollTick::Unchanged { hint: true });
        assert!(!stitcher.scrolled());
        assert_eq!(stitcher.appended(), 0);
    }

    #[test]
    fn fast_scroll_is_flagged_but_still_appended() {
        let page = page(64, 400, 17);
        let mut stitcher = Stitcher::new(viewport(&page, 0, 100), 4000);
        let delta = 60;
        assert_eq!(
            stitcher.tick(viewport(&page, delta, 100)),
            ScrollTick::Appended { fast: true }
        );
        assert_eq!(stitcher.appended(), delta);
    }

    #[test]
    fn match_failure_updates_the_reference_frame_without_appending() {
        let page_frame = page(64, 260, 0);
        let mut stitcher = Stitcher::new(viewport(&page_frame, 0, 120), 5000);
        let other = flat(64, 120, 200);
        assert_eq!(stitcher.tick(other), ScrollTick::NoMatch);
        assert_eq!(stitcher.appended(), 0);
        assert_eq!(stitcher.height(), 120);
    }

    #[test]
    fn reaching_the_cap_appends_remaining_rows_then_stops() {
        let page = page(48, 400, 9);
        let initial = viewport(&page, 0, 100);
        let cap = 100 + 30;
        let mut stitcher = Stitcher::new(initial, cap);
        let tick = stitcher.tick(viewport(&page, 50, 100));
        assert_eq!(tick, ScrollTick::LimitReached);
        assert!(stitcher.limit_reached());
        assert_eq!(stitcher.height(), cap);
        assert_eq!(stitcher.appended(), 30);
        assert_eq!(
            stitcher.tick(viewport(&page, 60, 100)),
            ScrollTick::LimitReached
        );
    }

    #[test]
    fn stitch_cap_shrinks_with_width_and_never_reaches_zero() {
        assert_eq!(stitch_cap_height(100), MAX_STITCH_HEIGHT);
        assert_eq!(
            stitch_cap_height(u32::MAX),
            (MAX_STITCH_PIXELS / u64::from(u32::MAX)).max(1) as u32
        );
        assert!(stitch_cap_height(0) > 0);
    }

    #[test]
    fn control_window_prefers_the_side_of_the_region() {
        let monitor = MonitorGeom::from_physical("m", 0, 0, 1920, 1080, 1.0);
        let region = RegionSelection {
            x: 100,
            y: 100,
            width: 400,
            height: 300,
        };
        let (x, y) = control_origin(&monitor, &region, (320.0, 150.0));
        assert_eq!((x, y), (100.0 + 400.0 + CONTROL_GAP, 100.0));
    }

    #[test]
    fn control_window_flips_left_when_right_side_is_occupied() {
        let monitor = MonitorGeom::from_physical("m", 0, 0, 1920, 1080, 1.0);
        let region = RegionSelection {
            x: 1500,
            y: 100,
            width: 400,
            height: 300,
        };
        let (x, _) = control_origin(&monitor, &region, (320.0, 150.0));
        assert_eq!(x, 1500.0 - 320.0 - CONTROL_GAP);
    }

    #[test]
    fn control_window_clamps_into_a_tiny_work_area() {
        let monitor = MonitorGeom::from_physical("m", 0, 0, 300, 200, 1.0);
        let region = RegionSelection {
            x: 0,
            y: 0,
            width: 300,
            height: 200,
        };
        let (x, y) = control_origin(&monitor, &region, (320.0, 150.0));
        assert_eq!((x, y), (0.0, 200.0 - 150.0 - CONTROL_GAP));
    }

    #[test]
    fn control_window_uses_physical_scale_for_logical_position() {
        let monitor = MonitorGeom::from_physical("m", 0, 0, 3840, 2160, 2.0);
        let region = RegionSelection {
            x: 200,
            y: 100,
            width: 800,
            height: 600,
        };
        let (x, y) = control_origin(&monitor, &region, (320.0, 150.0));
        assert_eq!((x, y), (100.0 + 400.0 + CONTROL_GAP, 50.0));
    }

    #[test]
    fn strip_height_caps_and_never_collapses() {
        assert_eq!(strip_height(1000), STRIP_HEIGHT);
        assert_eq!(strip_height(90), 30);
        assert_eq!(strip_height(0), 1);
    }

    #[test]
    fn scroll_status_serializes_for_the_control_window() {
        let json = serde_json::to_value(ScrollStatus::new("unchanged", 640, 1200, 90)).unwrap();
        assert_eq!(json["state"], "unchanged");
        assert_eq!(json["width"], 640);
        assert_eq!(json["height"], 1200);
        assert_eq!(json["appended"], 90);
    }
}

/// Windows 真机端到端冒烟(手动,`#[ignore]`):创建一个真实可滚动窗口,
/// 通过 WM_VSCROLL 逐步滚动并用 `platform::capture_monitor` 抓取屏幕,把
/// 真实抓屏帧喂给 `Stitcher`,验证「连续抓取 → 条带匹配 → 垂直拼接」在
/// Windows 上可用,且拼接结果与窗口内容逐像素一致。
#[cfg(all(test, windows))]
mod live_smoke {
    use super::*;
    use std::mem::size_of;

    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Dwm::DwmFlush;
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect,
        UpdateWindow, HDC, PAINTSTRUCT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        PostQuitMessage, RegisterClassExW, SendMessageW, SetForegroundWindow, ShowWindow,
        TranslateMessage, MSG, PM_REMOVE, SW_SHOW, WM_DESTROY, WM_PAINT, WM_VSCROLL, WNDCLASSEXW,
        WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    const WIN_X: i32 = 40;
    const WIN_Y: i32 = 40;
    const WIN_W: i32 = 360;
    const WIN_H: i32 = 260;
    const BAND: i32 = 40;
    const STEP: i32 = 90;
    const STEPS: u32 = 6;

    thread_local! {
        static SCROLL_POS: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    }

    /// 每个横向条的灰度值:相邻条相差 53,避免滚动位移出现周期性伪匹配。
    fn band_value(band: i32) -> u8 {
        ((band.rem_euclid(251) * 53) % 251) as u8
    }

    unsafe extern "system" fn proc_(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_VSCROLL => {
                SCROLL_POS.with(|pos| pos.set(pos.get() + STEP));
                let _ = InvalidateRect(Some(hwnd), None, true);
                let _ = UpdateWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn paint(hdc: HDC) {
        let pos = SCROLL_POS.with(|pos| pos.get());
        // 真实滚动:内容随 pos 上移,条带边界落在 40k - (pos mod 40)。
        let offset = pos.rem_euclid(BAND);
        let mut band = pos.div_euclid(BAND);
        let mut y = -offset;
        while y < WIN_H {
            let top = y.max(0);
            let bottom = (y + BAND).min(WIN_H);
            if bottom > top {
                let value = band_value(band);
                let brush = CreateSolidBrush(COLORREF(u32::from(value) * 0x0001_0101));
                let rect = RECT {
                    left: 0,
                    top,
                    right: WIN_W,
                    bottom,
                };
                let _ = FillRect(hdc, &rect, brush);
                let _ = DeleteObject(brush.into());
            }
            y += BAND;
            band += 1;
        }
    }

    fn pump() {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // 等一帧合成完成再抓屏,避免抓到半张旧画面(手动冒烟不追求速度)。
        std::thread::sleep(Duration::from_millis(150));
        unsafe {
            let _ = DwmFlush();
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    fn grab_window() -> Frame {
        let monitor = platform::pointer_monitor().expect("pointer monitor");
        let full = platform::capture_monitor(&monitor).expect("screen grab");
        crop_rgba(
            &full,
            (WIN_X - monitor.physical_x) as u32,
            (WIN_Y - monitor.physical_y) as u32,
            WIN_W as u32,
            WIN_H as u32,
        )
        .expect("window crop")
    }

    #[test]
    #[ignore = "manual: opens a real window and captures the live screen"]
    fn live_window_scroll_stitches_real_grabs() {
        unsafe {
            let instance = match GetModuleHandleW(None) {
                Ok(instance) => instance,
                Err(_) => {
                    eprintln!("skip: no module instance");
                    return;
                }
            };
            // 抓屏坐标是物理像素:测试进程必须先声明 DPI 感知,否则窗口
            // 坐标会被系统虚拟化,裁剪区域对不上。
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let class = w!("CropmarkScrollLiveSmoke");
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc_),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassExW(&wc) == 0 {
                eprintln!("skip: window class registration failed");
                return;
            }
            let Ok(hwnd) = CreateWindowExW(
                WS_EX_TOPMOST,
                class,
                w!("scroll smoke"),
                WS_POPUP | WS_VISIBLE,
                WIN_X,
                WIN_Y,
                WIN_W,
                WIN_H,
                None,
                None,
                Some(instance.into()),
                None,
            ) else {
                eprintln!("skip: window creation failed");
                return;
            };
            SCROLL_POS.with(|pos| pos.set(0));
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = UpdateWindow(hwnd);
            pump();

            let mut stitcher = Stitcher::new(grab_window(), 4000);
            for step in 0..STEPS {
                let _ = SendMessageW(hwnd, WM_VSCROLL, Some(WPARAM(0)), Some(LPARAM(0)));
                pump();
                let tick = stitcher.tick(grab_window());
                assert!(
                    matches!(tick, ScrollTick::Appended { .. }),
                    "step {step}: expected appended, got {tick:?}"
                );
            }
            let stitched = stitcher.into_frame();
            assert_eq!(stitched.height, WIN_H as u32 + STEP as u32 * STEPS);
            for row in (0..stitched.height).step_by(17) {
                let expected = band_value(row as i32 / BAND);
                let i = (row as usize * stitched.width as usize + 10) * 4;
                let pixel = &stitched.rgba[i..i + 3];
                assert!(
                    pixel.iter().all(|channel| *channel == expected),
                    "row {row}: {pixel:?} != {expected}"
                );
            }
            let _ = DestroyWindow(hwnd);
        }
    }
}
