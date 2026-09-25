//! R1 手动滚动长截图(ADR-5)。
//!
//! 选区壳确认固定物理区域后进入独立滚动会话:后端按约 300ms 周期抓取该
//! 区域,以「上一帧底部条带在当前帧中的垂直位置」估算滚动位移并垂直拼接;
//! Web 控制窗(`index.html?view=scroll`,置顶非模态、位于选区旁)承载状态
//! 提示与「完成/取消」。只支持垂直滚动:内容未变化连续若干次、匹配失败、
//! 滚动过快都给出可理解的状态提示并允许继续或取消;拼接高度设上限,达到
//! 上限自动完成并提示。
//!
//! 控制窗永不与采集区域相交(四侧放不下时改用其他显示器、收缩贴边,仍无
//! 合法位置则明确失败不开会话),并在支持排除抓取的平台(Windows/macOS)
//! 把控制窗从屏幕抓取中排除,避免置顶卡片污染拼接结果。
//!
//! 完成结果走 `session::finish_scroll_frame`(现有预览完成路径,历史与
//! 「上次区域」规则同源);取消、内容未变化、平台不支持三类路径给出提示且
//! 不写剪贴板、历史或磁盘。

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, Position, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder,
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

/// 控制窗期望逻辑尺寸。
const CONTROL_WIDTH: f64 = 320.0;
const CONTROL_HEIGHT: f64 = 150.0;
/// 收缩放置允许的最小可用逻辑尺寸(再小则改用其他显示器或明确失败)。
const MIN_CONTROL_WIDTH: f64 = 160.0;
const MIN_CONTROL_HEIGHT: f64 = 80.0;
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

/// `prev` 底部条带与 `next` 中 `candidate` 处同高条带的亮度差总和与采样数;
/// `step_y` 为纵向采样步长(粗扫 2、并列复核 1)。
fn strip_diff_sum(
    prev: &Frame,
    next: &Frame,
    strip_y: u32,
    candidate: u32,
    step_y: u32,
) -> (u64, u64) {
    let stride = prev.width as usize * 4;
    let strip_h = strip_height(prev.height);
    let step_y = step_y.max(1);
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
        sy += step_y;
    }
    (sum, count.max(1))
}

/// 平均亮度差(整数)。
fn strip_mean_diff(prev: &Frame, next: &Frame, strip_y: u32, candidate: u32, step_y: u32) -> u64 {
    let (sum, count) = strip_diff_sum(prev, next, strip_y, candidate, step_y);
    sum / count
}

/// 并列候选复核上限:超过该数量说明是同色/无纹理的大面积歧义,直接取最小
/// 位移,不再逐行复核(限制最坏耗时)。
const TIE_VERIFY_LIMIT: usize = 64;

/// 在 `next` 中寻找 `prev` 底部条带的垂直位置(只允许向上/不动,即只支持
/// 向下滚动内容):返回条带顶边 y(0..=prev.height-strip);平均亮度差超过
/// 容忍上限时返回 None。
///
/// 判定顺序保证静态纯色画面不会被误判为滚动:先看「无变化基线」(条带原位
/// `y=strip_y`,即位移 0)是否仍在容忍范围内,是则直接返回原位;否则粗扫
/// 全量候选,并列(相同最小均值,纯色/周期纹理下常见)的小集合改用逐行采样
/// 复核,消除小于纵向采样步长的伪并列,复核后仍并列时取最小位移(最大的
/// `y`),避免把首个精确匹配当成大幅滚动并合成出伪内容。
pub(crate) fn match_strip_offset(prev: &Frame, next: &Frame) -> Option<u32> {
    if prev.width == 0 || prev.height < 2 || next.width != prev.width || next.height != prev.height
    {
        return None;
    }
    let strip_y = prev.height - strip_height(prev.height);
    if strip_mean_diff(prev, next, strip_y, strip_y, SAMPLE_STEP_Y) <= MATCH_MAX_MEAN_DIFF {
        return Some(strip_y);
    }
    let mut best_mean = u64::MAX;
    let mut tied: Vec<u32> = Vec::new();
    for candidate in 0..=strip_y {
        let mean = strip_mean_diff(prev, next, strip_y, candidate, SAMPLE_STEP_Y);
        if mean < best_mean {
            best_mean = mean;
            tied.clear();
            tied.push(candidate);
        } else if mean == best_mean {
            tied.push(candidate);
        }
    }
    if best_mean > MATCH_MAX_MEAN_DIFF {
        return None;
    }
    if tied.len() > 1 && tied.len() <= TIE_VERIFY_LIMIT {
        // 逐行复核用未取整的差值总和比较:单个错位像素的均值会被整数除法
        // 抹成 0,只有总和比较才能把 ±1px 的伪并列剔除。
        let mut best_y = *tied.last().expect("tied is never empty");
        let (mut best_sum, mut best_count) = strip_diff_sum(prev, next, strip_y, best_y, 1);
        for candidate in tied.iter().rev().skip(1) {
            let (sum, count) = strip_diff_sum(prev, next, strip_y, *candidate, 1);
            if sum * best_count < best_sum * count {
                best_sum = sum;
                best_count = count;
                best_y = *candidate;
            }
        }
        return (best_sum <= MATCH_MAX_MEAN_DIFF * best_count).then_some(best_y);
    }
    Some(*tied.last().expect("tied is never empty"))
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

/// 控制窗最终放置:逻辑尺寸(建窗/改尺寸)与物理位置(最终定位,避免混合
/// DPI 下逻辑坐标换算漂移)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ControlPlacement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub physical_x: f64,
    pub physical_y: f64,
}

/// 物理像素矩形 `(x, y, width, height)`。
type PhysicalRect = (f64, f64, f64, f64);

fn monitor_scale(monitor: &MonitorGeom) -> f64 {
    if monitor.scale.is_finite() && monitor.scale > 0.0 {
        monitor.scale
    } else {
        1.0
    }
}

fn monitor_rect(monitor: &MonitorGeom) -> PhysicalRect {
    (
        monitor.physical_x as f64,
        monitor.physical_y as f64,
        monitor.physical_width as f64,
        monitor.physical_height as f64,
    )
}

/// 显示器逻辑全局坐标 + 逻辑尺寸 → 物理全局矩形(测试用于复核放置结果)。
#[cfg(test)]
fn physical_rect(monitor: &MonitorGeom, x: f64, y: f64, width: f64, height: f64) -> PhysicalRect {
    let scale = monitor_scale(monitor);
    (
        monitor.physical_x as f64 + (x - monitor.logical_x as f64) * scale,
        monitor.physical_y as f64 + (y - monitor.logical_y as f64) * scale,
        width * scale,
        height * scale,
    )
}

fn rect_contains(outer: PhysicalRect, inner: PhysicalRect) -> bool {
    inner.0 >= outer.0
        && inner.1 >= outer.1
        && inner.0 + inner.2 <= outer.0 + outer.2
        && inner.1 + inner.3 <= outer.1 + outer.3
}

fn rects_intersect(a: PhysicalRect, b: PhysicalRect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

fn placement_on(
    monitor: &MonitorGeom,
    physical_x: f64,
    physical_y: f64,
    width: f64,
    height: f64,
) -> ControlPlacement {
    let scale = monitor_scale(monitor);
    ControlPlacement {
        x: monitor.logical_x as f64 + (physical_x - monitor.physical_x as f64) / scale,
        y: monitor.logical_y as f64 + (physical_y - monitor.physical_y as f64) / scale,
        width,
        height,
        physical_x,
        physical_y,
    }
}

/// 控制窗放置:优先贴选区右侧,右侧放不下改左侧、下侧、上侧;都放不下时
/// 放到其他显示器;仍放不下则在本显示器上收缩到可用下限以上贴边。
///
/// 结果保证完整落在某台显示器内且**与采集区域不相交**(物理像素比较,混合
/// DPI 下不会把逻辑坐标当物理坐标误判)。没有任何合法位置时返回 None,调用
/// 方以明确文案失败,而不是把控制窗压进采集区域污染拼接结果。
pub(crate) fn control_placement(
    monitors: &[MonitorGeom],
    region_monitor: &MonitorGeom,
    region: &RegionSelection,
    window: (f64, f64),
) -> Option<ControlPlacement> {
    let (ww, wh) = window;
    let region_rect: PhysicalRect = (
        region_monitor.physical_x as f64 + region.x as f64,
        region_monitor.physical_y as f64 + region.y as f64,
        region.width as f64,
        region.height as f64,
    );
    let scale = monitor_scale(region_monitor);
    let gap = CONTROL_GAP * scale;
    let mrect = monitor_rect(region_monitor);
    let (rx, ry) = (region_rect.0, region_rect.1);
    let (rw, rh) = (region_rect.2, region_rect.3);
    let full_w = ww * scale;
    let full_h = wh * scale;
    let fits =
        |rect: PhysicalRect| rect_contains(mrect, rect) && !rects_intersect(rect, region_rect);

    // 1) 选区四侧,完整尺寸。
    for (x, y) in [
        (rx + rw + gap, ry),
        (rx - full_w - gap, ry),
        (rx, ry + rh + gap),
        (rx, ry - full_h - gap),
    ] {
        if fits((x, y, full_w, full_h)) {
            return Some(placement_on(region_monitor, x, y, ww, wh));
        }
    }

    // 2) 其他显示器(跳过镜像屏等同物理范围的屏幕):完整尺寸,靠近选区投影。
    let region_center = (rx + rw / 2.0, ry + rh / 2.0);
    for other in monitors {
        let orect = monitor_rect(other);
        if orect == mrect {
            continue;
        }
        let oscale = monitor_scale(other);
        let ow = ww * oscale;
        let oh = wh * oscale;
        let og = CONTROL_GAP * oscale;
        if orect.2 < ow + 2.0 * og || orect.3 < oh + 2.0 * og {
            continue;
        }
        let x = (region_center.0 - ow / 2.0).clamp(orect.0 + og, orect.0 + orect.2 - ow - og);
        let y = (region_center.1 - oh / 2.0).clamp(orect.1 + og, orect.1 + orect.3 - oh - og);
        if !rects_intersect((x, y, ow, oh), region_rect) {
            return Some(placement_on(other, x, y, ww, wh));
        }
    }

    // 3) 本显示器收缩贴边(不低于可用下限),顺序同四侧。
    let min_w = MIN_CONTROL_WIDTH * scale;
    let min_h = MIN_CONTROL_HEIGHT * scale;
    // 右侧:宽度受选区右边界限制。
    let width = (mrect.0 + mrect.2 - (rx + rw) - gap).min(full_w);
    if width >= min_w && full_h >= min_h && full_h + 2.0 * gap <= mrect.3 {
        let x = rx + rw + gap;
        let y = ry.clamp(mrect.1 + gap, mrect.1 + mrect.3 - full_h - gap);
        if fits((x, y, width, full_h)) {
            return Some(placement_on(region_monitor, x, y, width / scale, wh));
        }
    }
    // 左侧。
    let width = (rx - mrect.0 - gap).min(full_w);
    if width >= min_w && full_h >= min_h && full_h + 2.0 * gap <= mrect.3 {
        let x = rx - gap - width;
        let y = ry.clamp(mrect.1 + gap, mrect.1 + mrect.3 - full_h - gap);
        if fits((x, y, width, full_h)) {
            return Some(placement_on(region_monitor, x, y, width / scale, wh));
        }
    }
    // 下方:高度受选区下边界限制。
    let height = (mrect.1 + mrect.3 - (ry + rh) - gap).min(full_h);
    let width = full_w.min(mrect.2 - 2.0 * gap);
    if height >= min_h && width >= min_w {
        let y = ry + rh + gap;
        let x = rx.clamp(mrect.0 + gap, mrect.0 + mrect.2 - width - gap);
        if fits((x, y, width, height)) {
            return Some(placement_on(
                region_monitor,
                x,
                y,
                width / scale,
                height / scale,
            ));
        }
    }
    // 上方。
    let height = (ry - mrect.1 - gap).min(full_h);
    let width = full_w.min(mrect.2 - 2.0 * gap);
    if height >= min_h && width >= min_w {
        let y = ry - gap - height;
        let x = rx.clamp(mrect.0 + gap, mrect.0 + mrect.2 - width - gap);
        if fits((x, y, width, height)) {
            return Some(placement_on(
                region_monitor,
                x,
                y,
                width / scale,
                height / scale,
            ));
        }
    }
    None
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
    // 其他显示器用于控制窗兜底放置;枚举失败时退化为只考虑选区显示器,
    // 保证"放不下就明确失败"的不相交契约不变。
    let monitors = match session::tauri_monitors(app) {
        monitors if monitors.is_empty() => vec![monitor.clone()],
        monitors => monitors,
    };
    if let Err(error) = open_control_window(app, &monitors, &monitor, &region) {
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
    monitors: &[MonitorGeom],
    monitor: &MonitorGeom,
    region: &RegionSelection,
) -> Result<WebviewWindow, CaptureError> {
    let placement = control_placement(monitors, monitor, region, (CONTROL_WIDTH, CONTROL_HEIGHT))
        .ok_or_else(|| CaptureError::unavailable("error.capture.scroll_no_space"))?;
    let physical = Position::Physical(PhysicalPosition::new(
        placement.physical_x.round() as i32,
        placement.physical_y.round() as i32,
    ));
    // 复用已存在的控制窗(toast/error 同模式):关闭再同标签重建存在
    // webview 销毁竞态;隐藏复用只重置显示状态并通知前端补拉最新状态。
    if let Some(window) = app.get_webview_window(WINDOW) {
        // R1:控制窗从抓取中排除(Windows WDA_EXCLUDEFROMCAPTURE /
        // macOS sharingType=none;其他平台不支持时依赖不相交放置)。
        let _ = window.set_content_protected(true);
        let _ = window.set_size(LogicalSize::new(placement.width, placement.height));
        let _ = window.set_position(physical);
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
    .content_protected(true)
    .inner_size(placement.width, placement.height)
    .position(placement.x, placement.y)
    .build()
    .map_err(|error| CaptureError::api_detail("error.capture.window_build", &error.to_string()))?;
    // 物理坐标为准:混合 DPI 下逻辑位置换算可能有零点几像素漂移。
    let _ = window.set_position(physical);
    let _ = window.set_ignore_cursor_events(false);
    Ok(window)
}

/// 收起控制窗:仅隐藏,保留预创建 webview 供下一次会话复用。
fn dismiss_control_window_now(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.set_content_protected(false);
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

    /// 底部 `solid_rows` 行为纯色、其余为行纹的页面(模拟文档底部纯色留白)。
    fn page_with_solid_bottom(width: u32, height: u32, solid_rows: u32, value: u8) -> Frame {
        let mut frame = page(width, height, 0);
        let start = height.saturating_sub(solid_rows) as usize;
        for y in start..height as usize {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                frame.rgba[i] = value;
                frame.rgba[i + 1] = value;
                frame.rgba[i + 2] = value;
            }
        }
        frame
    }

    /// 每两行同值的粗纹理:纵向采样步长为 2 时,±1px 位移会伪装成并列精确匹配。
    fn coarse_page(width: u32, height: u32, seed: u32) -> Frame {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height {
            let value = (((y / 2) * 53 + seed) % 251) as u8;
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
    fn solid_static_frame_is_not_mistaken_for_a_scroll() {
        // 整区纯色:所有候选都精确命中,修复前取首个匹配(y=0)会伪造大幅位移。
        let frame = flat(64, 120, 200);
        let strip_y = 120 - strip_height(120);
        assert_eq!(match_strip_offset(&frame, &frame.clone()), Some(strip_y));

        let mut stitcher = Stitcher::new(frame.clone(), 1000);
        for tick in 0..(UNCHANGED_HINT_AFTER + 2) {
            let expected = ScrollTick::Unchanged {
                hint: tick + 1 >= UNCHANGED_HINT_AFTER,
            };
            assert_eq!(stitcher.tick(frame.clone()), expected, "tick {tick}");
        }
        assert!(!stitcher.scrolled());
        assert_eq!(stitcher.appended(), 0);
        assert!(!stitcher.limit_reached());
        assert_eq!(stitcher.height(), 120);
    }

    #[test]
    fn solid_bottom_band_static_frame_keeps_zero_delta() {
        // 底部纯色带比条带高:修复前首个精确匹配在 y=40,会追加 40 行伪内容。
        let page = page_with_solid_bottom(96, 200, 160, 230);
        let prev = viewport(&page, 0, 120);
        let strip_y = 120 - strip_height(120);
        assert_eq!(match_strip_offset(&prev, &prev.clone()), Some(strip_y));

        let mut stitcher = Stitcher::new(prev.clone(), 1000);
        assert_eq!(stitcher.tick(prev), ScrollTick::Unchanged { hint: false });
        assert!(!stitcher.scrolled());
        assert_eq!(stitcher.appended(), 0);
        assert_eq!(stitcher.height(), 120);
    }

    #[test]
    fn solid_bottom_band_does_not_mask_a_real_scroll() {
        // 纯色带上方仍有纹理:真实滚动时无变化基线不再匹配,位移仍可估计。
        let page = page_with_solid_bottom(96, 400, 160, 230);
        let prev = viewport(&page, 0, 120);
        let next = viewport(&page, 30, 120);
        let y = match_strip_offset(&prev, &next).expect("scrolled texture must match");
        assert_eq!(120 - strip_height(120) - y, 30);
    }

    #[test]
    fn tie_verification_rejects_parity_shifted_candidates() {
        // 粗纹理上 ±1px 候选在粗采样下与真实位移并列;逐行复核必须选回真实
        // 位移,而不是偏 1px 的合成结果。
        let page = coarse_page(96, 240, 5);
        let prev = viewport(&page, 0, 120);
        let strip_y = 120 - strip_height(120);
        for delta in [2u32, 9, 21] {
            let next = viewport(&page, delta, 120);
            let y = match_strip_offset(&prev, &next)
                .unwrap_or_else(|| panic!("delta {delta} must match"));
            assert_eq!(strip_y - y, delta, "delta {delta}");
        }
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

    fn placement(
        monitors: &[MonitorGeom],
        region_monitor: &MonitorGeom,
        region: &RegionSelection,
    ) -> Option<ControlPlacement> {
        control_placement(
            monitors,
            region_monitor,
            region,
            (CONTROL_WIDTH, CONTROL_HEIGHT),
        )
    }

    /// 放置结果必须完整落在目标显示器内且与采集区域零相交。
    fn assert_clear(
        placed: &ControlPlacement,
        target: &MonitorGeom,
        region_monitor: &MonitorGeom,
        region: &RegionSelection,
    ) {
        let window = physical_rect(target, placed.x, placed.y, placed.width, placed.height);
        let region_rect: PhysicalRect = (
            region_monitor.physical_x as f64 + region.x as f64,
            region_monitor.physical_y as f64 + region.y as f64,
            region.width as f64,
            region.height as f64,
        );
        assert!(
            rect_contains(monitor_rect(target), window),
            "control window {window:?} is outside its monitor"
        );
        assert!(
            !rects_intersect(window, region_rect),
            "control window {window:?} intersects the capture region {region_rect:?}"
        );
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
        let placed = placement(std::slice::from_ref(&monitor), &monitor, &region)
            .expect("room on the right");
        assert_eq!((placed.x, placed.y), (100.0 + 400.0 + CONTROL_GAP, 100.0));
        assert_eq!(
            (placed.width, placed.height),
            (CONTROL_WIDTH, CONTROL_HEIGHT)
        );
        assert_clear(&placed, &monitor, &monitor, &region);
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
        let placed =
            placement(std::slice::from_ref(&monitor), &monitor, &region).expect("room on the left");
        assert_eq!(placed.x, 1500.0 - CONTROL_WIDTH - CONTROL_GAP);
        assert_clear(&placed, &monitor, &monitor, &region);
    }

    #[test]
    fn fullscreen_region_on_a_single_monitor_has_no_placement() {
        // 选区占满唯一显示器:四侧、其他显示器、收缩都放不下,必须返回 None,
        // 由调用方明确失败而不是把控制窗塞进采集区域。
        let monitor = MonitorGeom::from_physical("m", 0, 0, 300, 200, 1.0);
        let region = RegionSelection {
            x: 0,
            y: 0,
            width: 300,
            height: 200,
        };
        assert_eq!(
            placement(std::slice::from_ref(&monitor), &monitor, &region),
            None
        );
    }

    #[test]
    fn near_fullscreen_region_shrinks_the_control_window_above_it() {
        let monitor = MonitorGeom::from_physical("m", 0, 0, 1920, 1080, 1.0);
        let region = RegionSelection {
            x: 0,
            y: 120,
            width: 1920,
            height: 960,
        };
        let placed = placement(std::slice::from_ref(&monitor), &monitor, &region)
            .expect("top margin must fit a shrunken card");
        assert_eq!((placed.x, placed.y), (CONTROL_GAP, 0.0));
        assert_eq!(placed.width, CONTROL_WIDTH);
        assert_eq!(placed.height, 120.0 - CONTROL_GAP);
        assert_clear(&placed, &monitor, &monitor, &region);
    }

    #[test]
    fn fullscreen_region_moves_the_control_window_to_another_monitor() {
        let primary = MonitorGeom::from_physical("p", 0, 0, 1920, 1080, 1.0);
        let secondary = MonitorGeom::from_physical("s", 1920, 0, 1920, 1080, 1.0);
        let monitors = [primary.clone(), secondary.clone()];
        let region = RegionSelection {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let placed = placement(&monitors, &primary, &region).expect("secondary display has room");
        assert_eq!(placed.physical_x, 1930.0);
        assert_clear(&placed, &secondary, &primary, &region);
    }

    #[test]
    fn mirrored_display_does_not_offer_placement() {
        // 镜像屏与选区共享物理范围:不能作为"其他显示器"兜底,应明确失败。
        let primary = MonitorGeom::from_physical("p", 0, 0, 1920, 1080, 1.0);
        let mirror = MonitorGeom::from_physical("mirror", 0, 0, 1920, 1080, 1.0);
        let region = RegionSelection {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            placement(&[primary.clone(), mirror], &primary, &region),
            None
        );
    }

    #[test]
    fn placement_compares_geometry_in_physical_pixels() {
        // 2x 缩放、选区只在物理方向留出顶部边距:收缩结果按物理像素判定不相交。
        let monitor = MonitorGeom::from_physical("retina", 0, 0, 3840, 2160, 2.0);
        let region = RegionSelection {
            x: 0,
            y: 240,
            width: 3840,
            height: 1800,
        };
        let placed = placement(std::slice::from_ref(&monitor), &monitor, &region)
            .expect("top physical margin must fit");
        assert_eq!((placed.x, placed.y), (CONTROL_GAP, 0.0));
        assert_eq!(placed.height, 110.0);
        assert_clear(&placed, &monitor, &monitor, &region);
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
        let placed = placement(std::slice::from_ref(&monitor), &monitor, &region)
            .expect("room on the right");
        assert_eq!((placed.x, placed.y), (100.0 + 400.0 + CONTROL_GAP, 50.0));
        assert_clear(&placed, &monitor, &monitor, &region);
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

    #[test]
    fn control_window_label_is_covered_by_the_default_capability() {
        // Tauri 2 按 (window label, capability windows) 放行 plugin 命令;
        // 控制窗前端的 listen('scroll-status'/'scroll-reload') 依赖 "scroll"
        // 出现在唯一 capability 的 windows 列表,缺失时会被 ACL 拒绝。
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../../capabilities/default.json"))
                .expect("default capability must be valid JSON");
        let windows = capability["windows"]
            .as_array()
            .expect("capability must list windows");
        assert!(
            windows.iter().any(|label| label == WINDOW),
            "capability windows {windows:?} must cover {WINDOW}"
        );
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
