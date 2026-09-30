//! R1 手动滚动长截图(ADR-5)。
//!
//! 选区壳确认固定物理区域后进入独立滚动会话:后端按约 100ms 周期抓取该
//! 区域,用整段模板的亮度差找唯一垂直位移再拼接。对不上或有两个差不多
//! 的候选时不追加,避免相似文本行被拼错行。
//! Web 控制窗(`index.html?view=scroll`,置顶非模态、位于选区旁)承载状态
//! 提示、方向选择与「完成/取消」。默认纵向(R5 前行为);横向在控制窗选择,
//! 首个内容变化后锁定。Windows 自动滚动的首个 nudge 推迟到控制窗就绪且
//! 方向选择落定(用户点选或就绪后过了宽限期)之后,避免在用户能选横向之前
//! 就把轴向锁死为纵向。内容未变化连续若干次、匹配失败、滚动过快都给出可
//! 理解的状态提示并允许继续或取消;沿滚动轴的拼接长度设上限,达到上限自动
//! 完成并提示。
//!
//! 控制窗永不与采集区域相交(四侧放不下时改用其他显示器、收缩贴边,仍无
//! 合法位置则明确失败不开会话),并在支持排除抓取的平台(Windows/macOS)
//! 把控制窗从屏幕抓取中排除,避免置顶卡片污染拼接结果。
//!
//! 完成结果走 `session::finish_scroll_frame`(现有预览完成路径,历史与
//! 「上次区域」规则同源);取消、内容未变化、平台不支持三类路径给出提示且
//! 不写剪贴板、历史或磁盘。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
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

#[cfg(windows)]
#[path = "scroll_highlight.rs"]
mod highlight;

/// 控制窗标签:滚动会话期间唯一,结束后隐藏复用(toast/error 同模式)。
pub const WINDOW: &str = "scroll";

/// 周期抓取间隔。约 100ms,正常滚动时相邻帧仍有大段重叠。
pub(crate) const CAPTURE_INTERVAL: Duration = Duration::from_millis(100);

/// 自动滚动宽限:控制窗就绪后留给用户选方向的时间;超时按当前(默认纵向)
/// 方向开始自动滚动,保持纵向默认体验。
#[cfg(any(windows, test))]
pub(crate) const AUTO_SCROLL_GRACE: Duration = Duration::from_millis(2_000);
/// 前端就绪信号迟迟不到时的兜底:超过该时限按控制窗已就绪处理,避免自动
/// 滚动永久停摆(无前端交互时仍沿用旧的自动滚动行为)。
#[cfg(any(windows, test))]
pub(crate) const AUTO_SCROLL_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// 对齐模板沿滚动轴的最大长度。取区域沿轴长度的 1/3,保证单帧还能识别
/// 超过半屏的位移。
pub(crate) const STRIP_LENGTH: u32 = 48;
/// 条带最多占区域沿轴长度的比例分母。
const STRIP_DIVISOR: u32 = 3;
/// 沿滚动轴的采样步长。调试构建里逐像素对齐会拖过抓取周期。
const SAMPLE_STEP: u32 = 4;
/// 滚动条不参与对齐:它不随内容移动,会把位移判成 0 或判歪。
/// 纵向滚动条在右侧,横向滚动条在底部。
const SCROLLBAR_GUTTER: u32 = 16;
/// 平均绝对亮度差上限(0–255)。超过则该候选不可信。
const MATCH_MAX_MEAN_DIFF: u64 = 18;
/// 最优比次优至少好这么多(平均亮度)才采纳。差一点的候选当成周期纹理,不拼接。
const MATCH_AMBIGUITY_GAP: u64 = 4;
/// 连续对不齐达到该次数后,才改用当前帧做基准(不追加)。一次失手不丢掉锚点。
const RESYNC_AFTER: u32 = 3;
/// 小于这个位移不当作滚动。光标闪一下也会让相邻帧差几个像素。
const MIN_APPEND_DELTA: u32 = 4;
/// 内容未变化连续达到该次数后给出提示(提示不终止会话)。
pub(crate) const UNCHANGED_HINT_AFTER: u32 = 12;
/// 沿滚动轴的拼接长度上限(物理像素):达到后自动完成并提示。
pub(crate) const MAX_STITCH_LENGTH: u32 = 12_000;
/// R2:可回退段数上限。每段多存一帧锚点,溢出丢弃最旧段——回退深度受限
/// 是接受取舍,更早的已拼接内容留在图里不再可回退。
const MAX_UNDO_SEGMENTS: usize = 64;
/// 拼接像素总量上限:限制超长区域的内存占用(长度上限随之收紧)。
pub(crate) const MAX_STITCH_PIXELS: u64 = 40_000_000;

/// 控制窗期望逻辑尺寸(含 R5 方向选择一行与开始按钮)。
const CONTROL_WIDTH: f64 = 320.0;
const CONTROL_HEIGHT: f64 = 232.0;
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
/// 控制窗请求的滚动轴:会话线程在下一次循环应用到拼接器;首个内容变化后
/// 请求被拒绝(见 `set_scroll_axis`/`Stitcher::set_axis`)。
static SCROLL_AXIS_REQUEST: AtomicU8 = AtomicU8::new(0);
/// 控制窗前端就绪信号:页面挂载并渲染出可交互的方向按钮后置位。Windows
/// 自动滚动在此之前不发送任何 nudge,保证用户能在首个内容变化前选方向。
static SCROLL_CONTROL_READY: AtomicBool = AtomicBool::new(false);
/// 控制窗是否已由用户显式选定方向(默认纵向下用户可能不点任何按钮)。
static SCROLL_AXIS_CHOSEN: AtomicBool = AtomicBool::new(false);
/// 就绪态的开始信号:确认选区后会话进入 `ready` 态,不自动滚动不拼接;
/// 开始按钮(控制卡/范围框触发)、范围框内滚轮推导开始或就绪期内容变化
/// 兜底任一到达后置位,方向随之锁定、Windows 自动 nudge 才开始按宽限
/// 规则发出。
pub(crate) static SCROLL_SESSION_STARTED: AtomicBool = AtomicBool::new(false);
/// R2:控制窗排队的回退次数。命令线程只计数,会话线程在下一轮循环串行
/// 执行 `Stitcher::undo_last_segment`,避免跨线程触碰拼接器。
static SCROLL_UNDO_REQUESTS: AtomicU64 = AtomicU64::new(0);

/// R5:长截图滚动轴。默认纵向;控制窗在首个内容变化前可切换为横向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaptureAxis {
    #[default]
    Vertical,
    Horizontal,
}

impl CaptureAxis {
    fn as_u8(self) -> u8 {
        match self {
            Self::Vertical => 0,
            Self::Horizontal => 1,
        }
    }

    fn from_u8(value: u8) -> Self {
        if value == 1 {
            Self::Horizontal
        } else {
            Self::Vertical
        }
    }
}

/// 控制窗状态载荷:前端按 `state` 映射词条,`axis` 显示方向与锁定状态,
/// `width`/`height` 为当前拼接结果尺寸(横向时宽度增长),`can_undo`
/// 表示回退栈非空(栈空时前端禁用回退按钮)。
/// `segment_count` 是当前拼接结果的构成段数(初始区域计 1 段):与
/// `appended`(像素数)口径不同,finishing 阶段的「正在拼接 · N 段」用它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrollStatus {
    pub state: String,
    pub axis: CaptureAxis,
    pub width: u32,
    pub height: u32,
    /// 已追加的像素数(不含初始区域)。
    pub appended: u32,
    /// 回退段栈非空:至少有一段可回退。
    pub can_undo: bool,
    /// 拼接结果总段数(初始区域 + 已追加段,回退同步扣减)。
    pub segment_count: u32,
}

impl ScrollStatus {
    fn new(
        state: &str,
        axis: CaptureAxis,
        width: u32,
        height: u32,
        appended: u32,
        can_undo: bool,
        segment_count: u32,
    ) -> Self {
        Self {
            state: state.to_string(),
            axis,
            width,
            height,
            appended,
            can_undo,
            segment_count,
        }
    }
}

/// 一次周期抓取的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollTick {
    /// 内容未变化;`hint` 表示连续未变化已达提示阈值。
    Unchanged { hint: bool },
    /// 已追加新内容;`fast` 表示位移超过区域沿轴长度一半(滚动过快提示)。
    Appended { fast: bool },
    /// 条带在当前帧中找不到可靠匹配:给出提示并允许继续/取消。
    NoMatch,
    /// 已达到拼接上限(已追加剩余部分),调用方应自动完成并提示。
    LimitReached,
}

/// 条带长度:不超过 `STRIP_LENGTH` 且不超过区域沿轴长度的 1/3。
pub(crate) fn strip_length(frame_length: u32) -> u32 {
    (frame_length / STRIP_DIVISOR).clamp(1, STRIP_LENGTH)
}

/// 采样/匹配使用的区域长度(纵向为高度,横向为宽度)。
fn axis_length(frame: &Frame, axis: CaptureAxis) -> u32 {
    match axis {
        CaptureAxis::Vertical => frame.height,
        CaptureAxis::Horizontal => frame.width,
    }
}

/// 模板条带在一帧中的起点(对齐模板取沿轴末尾)。
fn strip_start_offset(frame_length: u32) -> u32 {
    frame_length.saturating_sub(strip_length(frame_length))
}

/// 按另一条轴收紧后的拼接长度上限:纵向由宽度收紧,横向由高度收紧,
/// 两条轴共用同一像素预算。
fn stitch_cap_length(cross: u32) -> u32 {
    let cross = u64::from(cross.max(1));
    let by_pixels = (MAX_STITCH_PIXELS / cross).min(u64::from(MAX_STITCH_LENGTH));
    (by_pixels as u32).max(1)
}

/// 纵向滚动(垂直拼接)的高度上限。
pub(crate) fn stitch_cap_height(width: u32) -> u32 {
    stitch_cap_length(width)
}

/// 横向滚动(水平拼接)的宽度上限。
pub(crate) fn stitch_cap_width(height: u32) -> u32 {
    stitch_cap_length(height)
}

/// 按轴选择拼接长度上限。
pub(crate) fn stitch_cap(axis: CaptureAxis, initial: &Frame) -> u32 {
    match axis {
        CaptureAxis::Vertical => stitch_cap_height(initial.width),
        CaptureAxis::Horizontal => stitch_cap_width(initial.height),
    }
}

fn luma(px: &[u8]) -> u32 {
    (u32::from(px[0]) * 299 + u32::from(px[1]) * 587 + u32::from(px[2]) * 114) / 1000
}

/// 沿滚动轴逐线按步长采样的亮度。对齐只比较这些样本,避免逐像素拖慢
/// 调试构建。纵向一线 = 一行(跨 x 采样,忽略右侧滚动条);横向一线 =
/// 一列(跨 y 采样,忽略底部滚动条)。
#[derive(Debug, Clone, PartialEq, Eq)]
struct AxisSamples {
    n: usize,
    lines: Vec<u8>,
}

fn axis_samples(frame: &Frame, axis: CaptureAxis) -> AxisSamples {
    let step = SAMPLE_STEP.max(1);
    match axis {
        CaptureAxis::Vertical => {
            let gutter = if frame.width > SCROLLBAR_GUTTER + 8 {
                SCROLLBAR_GUTTER
            } else {
                0
            };
            let usable = frame.width - gutter;
            let n = usable.div_ceil(step).max(1) as usize;
            let mut lines = vec![0u8; frame.height as usize * n];
            let stride = frame.width as usize * 4;
            for y in 0..frame.height as usize {
                let src = y * stride;
                let dest = y * n;
                let mut i = 0usize;
                let mut x = 0u32;
                while x < usable && i < n {
                    let p = src + x as usize * 4;
                    lines[dest + i] = luma(&frame.rgba[p..p + 4]) as u8;
                    i += 1;
                    x += step;
                }
            }
            AxisSamples { n, lines }
        }
        CaptureAxis::Horizontal => {
            let gutter = if frame.height > SCROLLBAR_GUTTER + 8 {
                SCROLLBAR_GUTTER
            } else {
                0
            };
            let usable = frame.height - gutter;
            let n = usable.div_ceil(step).max(1) as usize;
            let mut lines = vec![0u8; frame.width as usize * n];
            let stride = frame.width as usize * 4;
            for x in 0..frame.width as usize {
                let dest = x * n;
                let mut i = 0usize;
                let mut y = 0u32;
                while y < usable && i < n {
                    let p = y as usize * stride + x * 4;
                    lines[dest + i] = luma(&frame.rgba[p..p + 4]) as u8;
                    i += 1;
                    y += step;
                }
            }
            AxisSamples { n, lines }
        }
    }
}

fn band_mean(
    prev: &AxisSamples,
    next: &AxisSamples,
    prev_line: u32,
    next_line: u32,
    lines: u32,
) -> u64 {
    let n = prev.n;
    let mut sum = 0u64;
    for dl in 0..lines as usize {
        let a = (prev_line as usize + dl) * n;
        let b = (next_line as usize + dl) * n;
        for i in 0..n {
            sum += u64::from(prev.lines[a + i].abs_diff(next.lines[b + i]));
        }
    }
    let count = (lines as u64) * (n as u64);
    sum / count.max(1)
}

/// 次优是否和最优拉开了差距。紧挨着的 ±2px 算同一处谷底,不拿来比。
fn match_is_distinct(costs: &[u64], best_at: usize) -> bool {
    let best = costs[best_at];
    let mut second = u64::MAX;
    for (index, cost) in costs.iter().enumerate() {
        if index.abs_diff(best_at) <= 2 {
            continue;
        }
        second = second.min(*cost);
    }
    if second == u64::MAX {
        return true;
    }
    let gap = second.saturating_sub(best);
    gap >= MATCH_AMBIGUITY_GAP || (second > best && gap.saturating_mul(4) >= best.max(1))
}

/// 在 `next` 中寻找 `prev` 末尾模板(纵向为底部条带、横向为右侧条带)的
/// 位置(只允许向滚动方向移动,即只支持向右/向下滚动):返回模板沿滚动轴
/// 的起点。没有足够好、且唯一的候选时返回 None,调用方不得按猜测的位移
/// 拼接。
///
/// 纯色或静止画面的零位移已经够好、又没有明显更优的别的位移时,直接判为
/// 没滚动。相似文本行会在多个位移上得到接近的分数,这种情况拒绝拼接。
pub(crate) fn match_strip_offset(prev: &Frame, next: &Frame, axis: CaptureAxis) -> Option<u32> {
    if prev.width == 0 || prev.height == 0 || next.width != prev.width || next.height != prev.height
    {
        return None;
    }
    let prev_length = axis_length(prev, axis);
    if prev_length < 2 {
        return None;
    }
    let template = strip_length(prev_length);
    let strip_start = prev_length - template;
    let prev_samples = axis_samples(prev, axis);
    let next_samples = axis_samples(next, axis);
    let mut costs = Vec::with_capacity(strip_start as usize + 1);
    for candidate in 0..=strip_start {
        costs.push(band_mean(
            &prev_samples,
            &next_samples,
            strip_start,
            candidate,
            template,
        ));
    }
    let zero_at = strip_start as usize;
    let zero = costs[zero_at];
    let mut best_at = 0usize;
    let mut best = u64::MAX;
    for (index, cost) in costs.iter().enumerate() {
        if *cost < best {
            best = *cost;
            best_at = index;
        }
    }
    if zero <= MATCH_MAX_MEAN_DIFF && best + MATCH_AMBIGUITY_GAP >= zero {
        return Some(strip_start);
    }
    if best > MATCH_MAX_MEAN_DIFF || !match_is_distinct(&costs, best_at) {
        return None;
    }
    Some(best_at as u32)
}

/// R2 回退段:一次 append 的像素量与追加前的锚帧。回退时恢复锚帧,
/// 让 RESYNC 换锚后的 prev 也一致回拨到该段追加前的状态。
#[derive(Debug)]
struct Segment {
    amount: u32,
    anchor: Frame,
}

/// 垂直拼接器:持有初始区域与最近一帧,按位移追加新内容。
pub(crate) struct Stitcher {
    axis: CaptureAxis,
    width: u32,
    scale: f64,
    rgba: Vec<u8>,
    height: u32,
    prev: Frame,
    /// per-append 回退栈(锚帧按追加前快照入栈;上限 `MAX_UNDO_SEGMENTS`)。
    segments: Vec<Segment>,
    /// 沿滚动轴的拼接长度上限(纵向 = 高度,横向 = 宽度)。
    cap_length: u32,
    appended: u32,
    /// 当前拼接结果的构成段数:初始区域计 1,每次追加 +1、回退 -1。
    /// 与回退栈独立维护——栈有 `MAX_UNDO_SEGMENTS` 上限会丢最旧段,
    /// 段数不能拿栈长推(否则超过上限后 finishing 的「N 段」会少报)。
    strips: u32,
    unchanged_ticks: u32,
    no_match_ticks: u32,
    scrolled: bool,
    limit_reached: bool,
}

impl Stitcher {
    pub(crate) fn new(initial: Frame, axis: CaptureAxis, cap_length: u32) -> Self {
        let width = initial.width;
        let scale = initial.scale;
        let height = initial.height;
        let rgba = initial.rgba.clone();
        Self {
            axis,
            width,
            scale,
            rgba,
            height,
            prev: initial,
            cap_length: cap_length.max(1),
            segments: Vec::new(),
            appended: 0,
            strips: 1,
            unchanged_ticks: 0,
            no_match_ticks: 0,
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

    pub(crate) fn axis(&self) -> CaptureAxis {
        self.axis
    }

    /// 沿滚动轴的当前拼接长度。
    fn axis_length(&self) -> u32 {
        match self.axis {
            CaptureAxis::Vertical => self.height,
            CaptureAxis::Horizontal => self.width,
        }
    }

    /// 切换滚动轴:只在还没有任何内容拼入时允许(首个内容变化后锁定);
    /// 成功时按新轴重算长度上限(纵横两轴共用同一像素预算)。会话层的
    /// 开始信号门在 `set_scroll_axis`,这里保持帧级守卫作为最后防线。
    pub(crate) fn set_axis(&mut self, axis: CaptureAxis) -> bool {
        if self.axis == axis {
            return true;
        }
        if self.appended != 0 || self.scrolled {
            return false;
        }
        self.axis = axis;
        self.cap_length = stitch_cap(axis, &self.prev);
        true
    }

    pub(crate) fn appended(&self) -> u32 {
        self.appended
    }

    /// 当前拼接结果的构成段数(初始区域 + 已追加段,回退同步扣减)。
    pub(crate) fn segment_count(&self) -> u32 {
        self.strips
    }

    /// 是否产生过滚动内容;为假时完成按钮按「内容未变化」路径处理(无输出)。
    pub(crate) fn scrolled(&self) -> bool {
        self.scrolled
    }

    pub(crate) fn limit_reached(&self) -> bool {
        self.limit_reached
    }

    /// 回退段栈非空(首帧不在栈里,天然不可回退)。
    pub(crate) fn can_undo(&self) -> bool {
        !self.segments.is_empty()
    }

    /// 当前对齐锚帧(就绪期兜底用它判断内容是否已变化,不消费帧)。
    pub(crate) fn anchor(&self) -> &Frame {
        &self.prev
    }

    /// 处理一帧新抓取的区域画面。
    pub(crate) fn tick(&mut self, next: Frame) -> ScrollTick {
        if self.axis_length() >= self.cap_length {
            self.limit_reached = true;
            return ScrollTick::LimitReached;
        }
        let prev_length = axis_length(&self.prev, self.axis);
        let strip_start = strip_start_offset(prev_length);
        let Some(position) = match_strip_offset(&self.prev, &next, self.axis) else {
            // 对不齐时先留着上一帧。用户放慢后仍能接上;连续失败才改锚点,不把错行写进去。
            self.no_match_ticks += 1;
            if self.no_match_ticks >= RESYNC_AFTER {
                self.prev = next;
                self.no_match_ticks = 0;
            }
            return ScrollTick::NoMatch;
        };
        self.no_match_ticks = 0;
        let delta = strip_start.saturating_sub(position);
        // 1–3px 多半是光标闪烁或抓屏抖动。记成一段会把「段数」打到几百，图也会错行。
        if delta < MIN_APPEND_DELTA {
            self.unchanged_ticks += 1;
            if delta == 0 {
                self.prev = next;
            }
            return ScrollTick::Unchanged {
                hint: self.unchanged_ticks >= UNCHANGED_HINT_AFTER,
            };
        }
        self.unchanged_ticks = 0;
        self.scrolled = true;
        let remaining = self.cap_length.saturating_sub(self.axis_length());
        let append = delta.min(remaining);
        if self.segments.len() >= MAX_UNDO_SEGMENTS {
            // 栈满丢弃最旧段:回退深度受限,更早已拼接的内容留在图里。
            self.segments.remove(0);
        }
        self.segments.push(Segment {
            amount: append,
            anchor: self.prev.clone(),
        });
        self.strips += 1;
        self.append_axis(&next, append);
        let fast = delta > prev_length / 2;
        self.prev = next;
        if append < delta || self.axis_length() >= self.cap_length {
            self.limit_reached = true;
            return ScrollTick::LimitReached;
        }
        ScrollTick::Appended { fast }
    }

    /// 沿滚动轴追加 `next` 的末尾 `amount` 像素(滚动 `amount` 像素后新增
    /// 的内容):纵向追加底部若干行,横向追加右侧若干列。
    fn append_axis(&mut self, next: &Frame, amount: u32) {
        if amount == 0 {
            return;
        }
        match self.axis {
            CaptureAxis::Vertical => {
                let stride = self.width as usize * 4;
                let start = (next.height - amount) as usize * stride;
                self.rgba
                    .extend_from_slice(&next.rgba[start..start + amount as usize * stride]);
                self.height += amount;
            }
            CaptureAxis::Horizontal => {
                let old_stride = self.width as usize * 4;
                let new_width = self.width + amount;
                let new_stride = new_width as usize * 4;
                let frame_stride = next.width as usize * 4;
                let take = amount as usize * 4;
                let mut rgba = vec![0u8; new_stride * self.height as usize];
                for y in 0..self.height as usize {
                    let dest = y * new_stride;
                    rgba[dest..dest + old_stride]
                        .copy_from_slice(&self.rgba[y * old_stride..(y + 1) * old_stride]);
                    let src = y * frame_stride + (next.width - amount) as usize * 4;
                    rgba[dest + old_stride..dest + new_stride]
                        .copy_from_slice(&next.rgba[src..src + take]);
                }
                self.rgba = rgba;
                self.width = new_width;
            }
        }
        self.appended += amount;
    }

    /// 回退最近一次追加:纵向截断尾部行、横向重建去掉末尾列,并同步回拨
    /// `appended`、锚帧 `prev`(恢复到该段追加前的帧,RESYNC 换锚也一致回拨)
    /// 与 `limit_reached`。栈空(只剩首帧)返回 false。回退后下一次拼接从
    /// 恢复的锚帧重新估计位移,段内已回退的内容按新位移正常续接。
    pub(crate) fn undo_last_segment(&mut self) -> bool {
        let Some(segment) = self.segments.pop() else {
            return false;
        };
        self.strips -= 1;
        match self.axis {
            CaptureAxis::Vertical => {
                let stride = self.width as usize * 4;
                let remove = segment.amount as usize * stride;
                self.rgba.truncate(self.rgba.len() - remove);
                self.height -= segment.amount;
            }
            CaptureAxis::Horizontal => {
                let old_stride = self.width as usize * 4;
                let new_width = self.width - segment.amount;
                let new_stride = new_width as usize * 4;
                let mut rgba = vec![0u8; new_stride * self.height as usize];
                for y in 0..self.height as usize {
                    rgba[y * new_stride..y * new_stride + new_stride]
                        .copy_from_slice(&self.rgba[y * old_stride..y * old_stride + new_stride]);
                }
                self.rgba = rgba;
                self.width = new_width;
            }
        }
        self.appended -= segment.amount;
        self.prev = segment.anchor;
        self.limit_reached = false;
        self.unchanged_ticks = 0;
        self.no_match_ticks = 0;
        true
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

/// 把会话推进到已开始:`started` 是方向锁定与自动 nudge 的门。
/// 三个触发源(开始按钮/框内滚轮/就绪期内容变化兜底)都走这里;
/// 幂等,重复触发只记第一次。
pub(crate) fn mark_session_started(app: &AppHandle, axis: CaptureAxis) {
    if SCROLL_SESSION_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    SCROLL_AXIS_REQUEST.store(axis.as_u8(), Ordering::SeqCst);
    if let Some(last) = get_scroll_status() {
        emit_status(app, ScrollStatus { state: "running".into(), ..last });
    }
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

/// 自动滚动开始门:首个自动 nudge 必须在控制窗可用于交互且方向选择落定之后。
///
/// - `axis_chosen`:用户已显式点选方向(点击本身就证明窗口可交互),立即放行;
/// - `since_ready`:控制窗就绪至今;宽限期内(`AUTO_SCROLL_GRACE`)不滚动,
///   让用户先选横向;
/// - 就绪信号缺失时(`None`)用会话开始时间兜底,超过
///   `AUTO_SCROLL_READY_TIMEOUT` 视为已就绪。
///
/// `started`:就绪态门,未开始时(按钮/滚轮/内容变化兜底都未到)一律不滚;
/// 开始后按原宽限规则给方向微调留时间,不再删除 nudge 机制。
///
/// 由此首个 append 不再可能发生在控制窗可交互之前,横向在自动滚动锁定前
/// 可达;用户不点方向时宽限到期仍按默认纵向自动滚动。
#[cfg(any(windows, test))]
pub(crate) fn auto_scroll_start_allowed(
    started: bool,
    axis_chosen: bool,
    since_start: Duration,
    since_ready: Option<Duration>,
) -> bool {
    if !started {
        return false;
    }
    if axis_chosen {
        return true;
    }
    match since_ready {
        Some(elapsed) => elapsed >= AUTO_SCROLL_GRACE,
        None => since_start >= AUTO_SCROLL_READY_TIMEOUT,
    }
}

/// R1:开始一次滚动会话。冻结帧与显示器几何从会话槽位读取;`axis` 为初始
/// 滚动方向(选区壳动作结果,默认纵向;控制窗可在首个内容变化前切换)。
/// 平台不支持时返回明确失败文案(不进入选区、不产出)。
pub(crate) fn start(
    app: &AppHandle,
    generation: u64,
    region: RegionSelection,
    annotations: Vec<Annotation>,
    axis: CaptureAxis,
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
    SCROLL_AXIS_REQUEST.store(axis.as_u8(), Ordering::SeqCst);
    SCROLL_AXIS_CHOSEN.store(false, Ordering::SeqCst);
    SCROLL_CONTROL_READY.store(false, Ordering::SeqCst);
    SCROLL_SESSION_STARTED.store(false, Ordering::SeqCst);
    SCROLL_UNDO_REQUESTS.store(0, Ordering::SeqCst);
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
    // 确认选区后先进入待开始态:不自动滚动不拼接,等待开始按钮、
    // 范围框内滚轮或就绪期内容变化兜底任一触发。
    emit_status(
        app,
        ScrollStatus::new("ready", axis, initial.width, initial.height, 0, false, 1),
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
                axis,
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
        reveal_scroll_control(&window);
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
    reveal_scroll_control(&window);
    Ok(window)
}

/// 控制窗只做提示,不能变成前台。前台必须回到被截的那个窗口,滚轮才能滚动它。
fn reveal_scroll_control(window: &WebviewWindow) {
    let _ = window.show();
    #[cfg(windows)]
    if let Ok(hwnd) = window.hwnd() {
        crate::capture::native_overlay::show_scroll_control_unfocused(hwnd);
    }
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
    axis: CaptureAxis,
    generation: u64,
) {
    let cap = stitch_cap(axis, &initial);
    let mut stitcher = Stitcher::new(initial, axis, cap);
    // 选区壳已经关掉。留一层点击穿透的范围框，让人看见正在截哪一块。
    #[cfg(windows)]
    let highlight = highlight::Guard::open(&monitor, &region, axis);
    #[cfg(windows)]
    let mut painted_length = stitcher.axis_length();
    // Windows 自动滚动的开闸状态:控制窗就绪且方向选择落定后,本循环才允许
    // 范围框定时器发送首个滚轮消息(见 `auto_scroll_start_allowed`)。
    #[cfg(windows)]
    let session_started = std::time::Instant::now();
    #[cfg(windows)]
    let mut control_ready_at: Option<std::time::Instant> = None;
    #[cfg(windows)]
    let mut auto_scroll_armed = false;
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
        // R2:回退请求按会话线程串行执行;回退成功则以最新尺寸/追加量
        // 推送一次状态,让控制卡的尺寸行与回退按钮同步回拨。
        let pending_undo = SCROLL_UNDO_REQUESTS.swap(0, Ordering::SeqCst);
        if pending_undo > 0 {
            let mut undone = false;
            for _ in 0..pending_undo {
                undone = stitcher.undo_last_segment() || undone;
            }
            if undone {
                let state = if SCROLL_SESSION_STARTED.load(Ordering::SeqCst) {
                    "running"
                } else {
                    "ready"
                };
                emit_status(&app, status_from(&stitcher, state));
            }
        }
        // R5:控制窗的方向选择在显式开始前自由切换;开始后锁定并把请求
        // 回退到实际方向,用状态事件把控制窗的选择回正。
        #[cfg(windows)]
        if !SCROLL_SESSION_STARTED.load(Ordering::SeqCst) {
            // 就绪期框内滚轮(低级钩子观察,不拦截):按滚轮方向推导轴向
            // 并开始;真实滚动由内容窗自己处理,本轮帧稍后兜底/拼接接管。
            if let Some(axis) = highlight::take_wheel_start_request() {
                mark_session_started(&app, axis);
            }
        }
        let requested = CaptureAxis::from_u8(SCROLL_AXIS_REQUEST.load(Ordering::SeqCst));
        if requested != stitcher.axis() {
            if stitcher.set_axis(requested) {
                #[cfg(windows)]
                if let Some(frame) = highlight.as_ref() {
                    frame.set_axis(requested);
                }
            } else {
                SCROLL_AXIS_REQUEST.store(stitcher.axis().as_u8(), Ordering::SeqCst);
            }
            let state = if SCROLL_SESSION_STARTED.load(Ordering::SeqCst) {
                "running"
            } else {
                "ready"
            };
            emit_status(&app, status_from(&stitcher, state));
        }
        #[cfg(windows)]
        if !auto_scroll_armed {
            // 方向请求先于开闸应用(上一段):用户点选后立即按新方向开闸,
            // 首个自动 nudge 不会用旧方向抢跑。
            if control_ready_at.is_none() && SCROLL_CONTROL_READY.load(Ordering::SeqCst) {
                control_ready_at = Some(std::time::Instant::now());
            }
            let chosen = SCROLL_AXIS_CHOSEN.load(Ordering::SeqCst);
            let since_ready = control_ready_at.map(|ready| ready.elapsed());
            let started = SCROLL_SESSION_STARTED.load(Ordering::SeqCst);
            if auto_scroll_start_allowed(
                started,
                chosen,
                session_started.elapsed(),
                since_ready,
            ) {
                if highlight.is_some() {
                    highlight::arm_auto_scroll();
                }
                auto_scroll_armed = true;
            }
        }
        std::thread::sleep(CAPTURE_INTERVAL);
        if SCROLL_STATE.load(Ordering::SeqCst) != STATE_RUNNING {
            continue;
        }
        let full = match platform::capture_monitor(&monitor) {
            Ok(frame) => frame,
            Err(_) => {
                emit_status(&app, status_from(&stitcher, "failed"));
                continue;
            }
        };
        let next = match crop_rgba(&full, region.x, region.y, region.width, region.height) {
            Ok(frame) => frame,
            Err(_) => {
                emit_status(&app, status_from(&stitcher, "failed"));
                continue;
            }
        };
        if !SCROLL_SESSION_STARTED.load(Ordering::SeqCst) {
            // 就绪期兜底:内容已变化(用户在框内滚了轮而钩子/范围框没接到,
            // 或非 Windows 平台直接手动滚动)即置 started 并锁方向,沿用
            // 当前方向开始拼接;未变化时保持 ready 提示,不拼接不滚动。
            if match_strip_offset(stitcher.anchor(), &next, stitcher.axis()).is_some_and(
                |position| {
                    let length = axis_length(stitcher.anchor(), stitcher.axis());
                    strip_start_offset(length).saturating_sub(position) >= MIN_APPEND_DELTA
                },
            ) {
                mark_session_started(&app, stitcher.axis());
            } else {
                emit_status(&app, status_from(&stitcher, "ready"));
                continue;
            }
        }
        match stitcher.tick(next) {
            ScrollTick::Unchanged { hint } => emit_status(
                &app,
                status_from(&stitcher, if hint { "unchanged" } else { "running" }),
            ),
            ScrollTick::Appended { fast } => emit_status(
                &app,
                status_from(&stitcher, if fast { "fast" } else { "running" }),
            ),
            ScrollTick::NoMatch => emit_status(&app, status_from(&stitcher, "no_match")),
            ScrollTick::LimitReached => {
                emit_status(&app, status_from(&stitcher, "limit"));
                break;
            }
        }
        #[cfg(windows)]
        if let Some(frame) = highlight.as_ref() {
            let stitched = stitcher.axis_length();
            if stitched != painted_length {
                frame.update(stitched);
                painted_length = stitched;
            }
        }
    }
    SCROLL_STATE.store(STATE_FINISHING, Ordering::SeqCst);
    emit_status(&app, status_from(&stitcher, "finishing"));
    finish_session(&app, stitcher, &annotations, &region, generation);
}

/// 由拼接器当前状态构造控制窗状态载荷(方向与尺寸都以拼接器为准)。
fn status_from(stitcher: &Stitcher, state: &str) -> ScrollStatus {
    ScrollStatus::new(
        state,
        stitcher.axis(),
        stitcher.width(),
        stitcher.height(),
        stitcher.appended(),
        stitcher.can_undo(),
        stitcher.segment_count(),
    )
}

/// 完成会话:先置回 IDLE 再收起控制窗(窗口销毁事件在 IDLE 状态下不触发
/// 取消),未产生滚动内容时按「内容未变化」路径提示且不产出;达到上限时
/// 附加提示(按滚动方向给出高度/宽度文案)。结果走现有预览完成路径。
fn finish_session(
    app: &AppHandle,
    stitcher: Stitcher,
    annotations: &[Annotation],
    region: &RegionSelection,
    generation: u64,
) {
    let axis = stitcher.axis();
    let scrolled = stitcher.scrolled();
    let limit = stitcher.limit_reached();
    let watched_width = stitcher.width().to_string();
    let watched_height = stitcher.height().to_string();
    let frame = stitcher.into_frame();
    release_state(generation);
    dismiss_control_window(app, generation);
    if !scrolled {
        session::cancel_scroll_session(app, generation);
        let key = match axis {
            CaptureAxis::Vertical => "toast.scroll_no_change",
            CaptureAxis::Horizontal => "toast.scroll_no_change.horizontal",
        };
        super::ui::show_toast_key_params(
            app,
            key,
            &[("width", &watched_width), ("height", &watched_height)],
        );
        return;
    }
    match session::finish_scroll_frame(app, frame, annotations.to_vec(), region, generation) {
        Ok(()) => {
            if limit {
                let key = match axis {
                    CaptureAxis::Vertical => "toast.scroll_limit",
                    CaptureAxis::Horizontal => "toast.scroll_limit.horizontal",
                };
                super::ui::show_toast_key(app, key);
            }
        }
        Err(error) if !error.is_cancelled() => {
            super::ui::show_toast(app, &error.user_message());
        }
        Err(_) => {}
    }
}

/// 控制窗前端消息:显式开始。就绪态下置 started(方向随之锁定、拼接与
/// 自动 nudge 开始生效);已开始后是幂等 no-op。
#[tauri::command]
pub fn start_scroll_capture(app: AppHandle) -> Result<(), CaptureError> {
    if SCROLL_STATE.load(Ordering::SeqCst) != STATE_RUNNING {
        return Err(CaptureError::api("error.capture.scroll_missing"));
    }
    let axis = CaptureAxis::from_u8(SCROLL_AXIS_REQUEST.load(Ordering::SeqCst));
    mark_session_started(&app, axis);
    Ok(())
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
            let last = get_scroll_status().unwrap_or_else(|| {
                ScrollStatus::new("running", CaptureAxis::default(), 0, 0, 0, false, 1)
            });
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

/// 控制窗前端消息:回退最近一段拼接(仅运行中且未进入完成流程可调;
/// 就绪态栈为空时为 no-op)。实际回退由会话线程在下一轮循环执行。
#[tauri::command]
pub fn undo_scroll_segment() -> Result<(), CaptureError> {
    if SCROLL_STATE.load(Ordering::SeqCst) != STATE_RUNNING {
        return Err(CaptureError::api("error.capture.scroll_missing"));
    }
    SCROLL_UNDO_REQUESTS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// 控制窗前端消息:在显式开始前切换滚动方向;开始后(按钮/滚轮/内容
/// 变化兜底任一触发)拒绝,控制窗同时按状态事件里的 `axis` 锁定选择。
#[tauri::command]
pub fn set_scroll_axis(axis: CaptureAxis) -> Result<(), CaptureError> {
    if !axis_switch_allowed(
        SCROLL_STATE.load(Ordering::SeqCst),
        SCROLL_SESSION_STARTED.load(Ordering::SeqCst),
    ) {
        return Err(CaptureError::api("error.capture.scroll_axis_locked"));
    }
    SCROLL_AXIS_REQUEST.store(axis.as_u8(), Ordering::SeqCst);
    // 用户点选方向即视为窗口可交互:自动滚动不必再等宽限期(见
    // `auto_scroll_start_allowed`)。
    SCROLL_AXIS_CHOSEN.store(true, Ordering::SeqCst);
    Ok(())
}

/// 方向切换窗口:会话运行中且还没显式开始(锁定锚点从首个 append
/// 移到开始信号,就绪期内方向可自由切换)。
fn axis_switch_allowed(state: u8, started: bool) -> bool {
    state == STATE_RUNNING && !started
}

/// 控制窗前端消息:控制窗已挂载并渲染出可交互状态(方向按钮可用)。
/// Windows 自动滚动在此之前不发送首个 nudge,保证用户能先选方向。
#[tauri::command]
pub fn scroll_control_ready() {
    SCROLL_CONTROL_READY.store(true, Ordering::SeqCst);
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

    /// 列主频图案:相邻列亮度差远大于容忍上限,保证横向匹配唯一。
    fn wide_page(width: u32, height: u32, seed: u32) -> Frame {
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for x in 0..width {
            let value = ((x * 53 + seed) % 251) as u8;
            for y in 0..height {
                let i = (y as usize * width as usize + x as usize) * 4;
                let jitter = (y % 3) as u8;
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

    /// 横向视口:从页面 `left` 起截取 `width` 列(1:1 像素切片)。
    fn horizontal_viewport(page: &Frame, left: u32, width: u32) -> Frame {
        crop_rgba(page, left, 0, width, page.height).unwrap()
    }

    #[test]
    fn bottom_strip_matching_estimates_scroll_delta() {
        let page = page(96, 240, 0);
        let prev = viewport(&page, 0, 120);
        for delta in [1u32, 8, 37, 56] {
            let next = viewport(&page, delta, 120);
            let y = match_strip_offset(&prev, &next, CaptureAxis::Vertical)
                .unwrap_or_else(|| panic!("delta {delta} must match"));
            let strip_y = 120 - strip_length(120);
            assert_eq!(strip_y - y, delta, "delta {delta}");
        }
    }

    #[test]
    fn unchanged_viewport_matches_at_rest_with_zero_delta() {
        let page = page(64, 160, 7);
        let prev = viewport(&page, 0, 120);
        let y = match_strip_offset(&prev, &prev.clone(), CaptureAxis::Vertical)
            .expect("identical frames match");
        assert_eq!(y, 120 - strip_length(120));
    }

    #[test]
    fn matching_fails_when_content_has_no_shared_band() {
        let prev = page(64, 120, 0);
        let next = flat(64, 120, 120);
        assert_eq!(
            match_strip_offset(&prev, &next, CaptureAxis::Vertical),
            None
        );
        assert_eq!(
            match_strip_offset(&prev, &next, CaptureAxis::Horizontal),
            None
        );
    }

    #[test]
    fn solid_static_frame_is_not_mistaken_for_a_scroll() {
        // 整区纯色:所有候选都精确命中,修复前取首个匹配(y=0)会伪造大幅位移。
        let frame = flat(64, 120, 200);
        let strip_y = 120 - strip_length(120);
        assert_eq!(
            match_strip_offset(&frame, &frame.clone(), CaptureAxis::Vertical),
            Some(strip_y)
        );

        let mut stitcher = Stitcher::new(frame.clone(), CaptureAxis::Vertical, 1000);
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
        let strip_y = 120 - strip_length(120);
        assert_eq!(
            match_strip_offset(&prev, &prev.clone(), CaptureAxis::Vertical),
            Some(strip_y)
        );

        let mut stitcher = Stitcher::new(prev.clone(), CaptureAxis::Vertical, 1000);
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
        let y = match_strip_offset(&prev, &next, CaptureAxis::Vertical)
            .expect("scrolled texture must match");
        assert_eq!(120 - strip_length(120) - y, 30);
    }

    #[test]
    fn tie_verification_rejects_parity_shifted_candidates() {
        // 粗纹理上 ±1px 候选在粗采样下与真实位移并列;逐行复核必须选回真实
        // 位移,而不是偏 1px 的合成结果。
        let page = coarse_page(96, 240, 5);
        let prev = viewport(&page, 0, 120);
        let strip_y = 120 - strip_length(120);
        for delta in [2u32, 9, 21] {
            let next = viewport(&page, delta, 120);
            let y = match_strip_offset(&prev, &next, CaptureAxis::Vertical)
                .unwrap_or_else(|| panic!("delta {delta} must match"));
            assert_eq!(strip_y - y, delta, "delta {delta}");
        }
    }

    #[test]
    fn stitcher_appends_exactly_the_new_rows_after_a_scroll() {
        let page = page(80, 300, 3);
        let initial = viewport(&page, 0, 120);
        let mut stitcher = Stitcher::new(initial, CaptureAxis::Vertical, stitch_cap_height(80));
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
        let mut stitcher = Stitcher::new(viewport(&page, 0, 100), CaptureAxis::Vertical, 1000);
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
        let mut stitcher = Stitcher::new(initial.clone(), CaptureAxis::Vertical, 1000);
        for _ in 1..UNCHANGED_HINT_AFTER {
            assert_eq!(
                stitcher.tick(initial.clone()),
                ScrollTick::Unchanged { hint: false }
            );
        }
        assert_eq!(stitcher.tick(initial), ScrollTick::Unchanged { hint: true });
        assert!(!stitcher.scrolled());
        assert_eq!(stitcher.appended(), 0);
    }

    #[test]
    fn fast_scroll_is_flagged_but_still_appended() {
        let page = page(64, 400, 17);
        let mut stitcher = Stitcher::new(viewport(&page, 0, 100), CaptureAxis::Vertical, 4000);
        let delta = 60;
        assert_eq!(
            stitcher.tick(viewport(&page, delta, 100)),
            ScrollTick::Appended { fast: true }
        );
        assert_eq!(stitcher.appended(), delta);
    }

    #[test]
    fn match_failure_keeps_the_anchor_so_the_next_good_frame_still_joins() {
        let page_frame = page(64, 260, 0);
        let mut stitcher =
            Stitcher::new(viewport(&page_frame, 0, 120), CaptureAxis::Vertical, 5000);
        let other = flat(64, 120, 200);
        assert_eq!(stitcher.tick(other.clone()), ScrollTick::NoMatch);
        assert_eq!(stitcher.appended(), 0);
        assert_eq!(stitcher.height(), 120);
        // 一次对不齐不换基准:接下来的真实滚动仍按最初那一帧拼接。
        assert_eq!(
            stitcher.tick(viewport(&page_frame, 20, 120)),
            ScrollTick::Appended { fast: false }
        );
        assert_eq!(stitcher.height(), 140);
        // 连续对不齐才改锚点,并且仍然不追加。
        let mut stuck = Stitcher::new(viewport(&page_frame, 0, 120), CaptureAxis::Vertical, 5000);
        for _ in 0..RESYNC_AFTER {
            assert_eq!(stuck.tick(other.clone()), ScrollTick::NoMatch);
        }
        assert_eq!(stuck.appended(), 0);
        assert_eq!(stuck.height(), 120);
    }

    /// 每一行都像文本:共享竖向节奏,但行与行的内容不同。错一行时亮度差必须
    /// 明显大于真正对齐,结果要和原页面逐像素一致。
    #[test]
    fn similar_text_rows_stitch_on_the_true_line_not_a_neighbor() {
        let line = 16u32;
        let width = 96u32;
        let height = 320u32;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height {
            let line_i = y / line;
            let local = y % line;
            for x in 0..width {
                let stem = x % 8 < 2;
                let unique =
                    ((line_i.wrapping_mul(31).wrapping_add(x.wrapping_mul(3))) % 160) as u8;
                let value = if local < 3 {
                    if stem {
                        36
                    } else {
                        214
                    }
                } else if local + 3 >= line {
                    228
                } else {
                    unique
                };
                let i = (y as usize * width as usize + x as usize) * 4;
                rgba[i] = value;
                rgba[i + 1] = value;
                rgba[i + 2] = value;
                rgba[i + 3] = 255;
            }
        }
        let document = Frame {
            width,
            height,
            rgba,
            scale: 1.0,
        };
        let mut stitcher = Stitcher::new(viewport(&document, 0, 96), CaptureAxis::Vertical, 4000);
        let mut top = 0u32;
        for step in [line, line * 2, line, line * 3] {
            top += step;
            let tick = stitcher.tick(viewport(&document, top, 96));
            assert_eq!(tick, ScrollTick::Appended { fast: false }, "top {top}");
        }
        let stitched = stitcher.into_frame();
        let expected = crop_rgba(&document, 0, 0, width, 96 + top).unwrap();
        assert_eq!(stitched.rgba, expected.rgba);
    }

    #[test]
    fn reaching_the_cap_appends_remaining_rows_then_stops() {
        let page = page(48, 400, 9);
        let initial = viewport(&page, 0, 100);
        let cap = 100 + 30;
        let mut stitcher = Stitcher::new(initial, CaptureAxis::Vertical, cap);
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
    fn horizontal_strip_matching_estimates_scroll_delta() {
        let page = wide_page(400, 96, 0);
        let prev = horizontal_viewport(&page, 0, 120);
        for delta in [1u32, 8, 37, 56] {
            let next = horizontal_viewport(&page, delta, 120);
            let x = match_strip_offset(&prev, &next, CaptureAxis::Horizontal)
                .unwrap_or_else(|| panic!("delta {delta} must match"));
            let strip_x = 120 - strip_length(120);
            assert_eq!(strip_x - x, delta, "delta {delta}");
        }
    }

    #[test]
    fn horizontal_stitcher_appends_exactly_the_new_columns() {
        let page = wide_page(300, 80, 3);
        let initial = horizontal_viewport(&page, 0, 120);
        let mut stitcher = Stitcher::new(initial, CaptureAxis::Horizontal, 10_000);
        let delta = 45;
        let tick = stitcher.tick(horizontal_viewport(&page, delta, 120));
        assert_eq!(tick, ScrollTick::Appended { fast: false });
        assert_eq!(stitcher.width(), 120 + delta);
        assert_eq!(stitcher.height(), 80);
        assert_eq!(stitcher.appended(), delta);
        assert!(stitcher.scrolled());

        let stitched = stitcher.into_frame();
        let expected = crop_rgba(&page, 0, 0, 120 + delta, 80).unwrap();
        assert_eq!(stitched.width, expected.width);
        assert_eq!(stitched.height, expected.height);
        assert_eq!(stitched.rgba, expected.rgba);
    }

    #[test]
    fn horizontal_stitcher_joins_multiple_scroll_steps_without_gaps() {
        let page = wide_page(400, 72, 11);
        let mut stitcher = Stitcher::new(
            horizontal_viewport(&page, 0, 100),
            CaptureAxis::Horizontal,
            1000,
        );
        let mut left = 0u32;
        for delta in [20u32, 7, 33, 12] {
            left += delta;
            assert_eq!(
                stitcher.tick(horizontal_viewport(&page, left, 100)),
                ScrollTick::Appended { fast: false }
            );
        }
        let stitched = stitcher.into_frame();
        let expected = crop_rgba(&page, 0, 0, 100 + left, 72).unwrap();
        assert_eq!(stitched.rgba, expected.rgba);
    }

    #[test]
    fn horizontal_unchanged_ticks_report_no_motion_and_hint_after_threshold() {
        let page = wide_page(300, 64, 5);
        let initial = horizontal_viewport(&page, 0, 100);
        assert_eq!(
            match_strip_offset(&initial, &initial, CaptureAxis::Horizontal),
            Some(100 - strip_length(100))
        );
        let mut stitcher = Stitcher::new(initial.clone(), CaptureAxis::Horizontal, 1000);
        for _ in 1..UNCHANGED_HINT_AFTER {
            assert_eq!(
                stitcher.tick(initial.clone()),
                ScrollTick::Unchanged { hint: false }
            );
        }
        assert_eq!(stitcher.tick(initial), ScrollTick::Unchanged { hint: true });
        assert!(!stitcher.scrolled());
        assert_eq!(stitcher.appended(), 0);
        assert_eq!(stitcher.width(), 100);
    }

    #[test]
    fn horizontal_fast_scroll_is_flagged_but_still_appended() {
        let page = wide_page(400, 64, 17);
        let mut stitcher = Stitcher::new(
            horizontal_viewport(&page, 0, 100),
            CaptureAxis::Horizontal,
            4000,
        );
        let delta = 60;
        assert_eq!(
            stitcher.tick(horizontal_viewport(&page, delta, 100)),
            ScrollTick::Appended { fast: true }
        );
        assert_eq!(stitcher.appended(), delta);
    }

    #[test]
    fn horizontal_reaching_the_cap_appends_remaining_columns_then_stops() {
        let page = wide_page(400, 48, 9);
        let initial = horizontal_viewport(&page, 0, 100);
        let cap = 100 + 30;
        let mut stitcher = Stitcher::new(initial, CaptureAxis::Horizontal, cap);
        let tick = stitcher.tick(horizontal_viewport(&page, 50, 100));
        assert_eq!(tick, ScrollTick::LimitReached);
        assert!(stitcher.limit_reached());
        assert_eq!(stitcher.width(), cap);
        assert_eq!(stitcher.appended(), 30);
        assert_eq!(
            stitcher.tick(horizontal_viewport(&page, 60, 100)),
            ScrollTick::LimitReached
        );
    }

    /// 每一列都像文本:共享横向节奏,但列与列的内容不同。错一列时亮度差
    /// 必须明显大于真正对齐,结果要和原页面逐像素一致(对齐按列而非按行)。
    #[test]
    fn horizontal_similar_bands_stitch_on_the_true_column_not_a_neighbor() {
        let band = 16u32;
        let width = 320u32;
        let height = 96u32;
        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        for x in 0..width {
            let band_i = x / band;
            let local = x % band;
            for y in 0..height {
                let stem = y % 8 < 2;
                let unique =
                    ((band_i.wrapping_mul(31).wrapping_add(y.wrapping_mul(3))) % 160) as u8;
                let value = if local < 3 {
                    if stem {
                        36
                    } else {
                        214
                    }
                } else if local + 3 >= band {
                    228
                } else {
                    unique
                };
                let i = (y as usize * width as usize + x as usize) * 4;
                rgba[i] = value;
                rgba[i + 1] = value;
                rgba[i + 2] = value;
                rgba[i + 3] = 255;
            }
        }
        let document = Frame {
            width,
            height,
            rgba,
            scale: 1.0,
        };
        let mut stitcher = Stitcher::new(
            horizontal_viewport(&document, 0, 96),
            CaptureAxis::Horizontal,
            4000,
        );
        let mut left = 0u32;
        for step in [band, band * 2, band, band * 3] {
            left += step;
            let tick = stitcher.tick(horizontal_viewport(&document, left, 96));
            assert_eq!(tick, ScrollTick::Appended { fast: false }, "left {left}");
        }
        let stitched = stitcher.into_frame();
        let expected = crop_rgba(&document, 0, 0, 96 + left, height).unwrap();
        assert_eq!(stitched.rgba, expected.rgba);
    }

    #[test]
    fn alignment_samples_ignore_the_scrollbar_gutter_on_each_axis() {
        let frame = wide_page(160, 120, 9);
        let horizontal = axis_samples(&frame, CaptureAxis::Horizontal);
        assert_eq!(
            horizontal.n,
            (frame.height - SCROLLBAR_GUTTER).div_ceil(SAMPLE_STEP) as usize
        );
        let mut bottom_bar = frame.clone();
        for y in (frame.height - SCROLLBAR_GUTTER)..frame.height {
            for x in 0..frame.width {
                let i = (y as usize * frame.width as usize + x as usize) * 4;
                bottom_bar.rgba[i..i + 4].copy_from_slice(&[20, 200, 90, 255]);
            }
        }
        assert_eq!(
            axis_samples(&bottom_bar, CaptureAxis::Horizontal),
            horizontal
        );

        let vertical = axis_samples(&frame, CaptureAxis::Vertical);
        assert_eq!(
            vertical.n,
            (frame.width - SCROLLBAR_GUTTER).div_ceil(SAMPLE_STEP) as usize
        );
        let mut right_bar = frame.clone();
        for y in 0..frame.height {
            for x in (frame.width - SCROLLBAR_GUTTER)..frame.width {
                let i = (y as usize * frame.width as usize + x as usize) * 4;
                right_bar.rgba[i..i + 4].copy_from_slice(&[20, 200, 90, 255]);
            }
        }
        assert_eq!(axis_samples(&right_bar, CaptureAxis::Vertical), vertical);

        // 可用区域内的改动必须反映到样本上:忽略的只是滚动条。
        let mut changed = frame.clone();
        changed.rgba[0..4].copy_from_slice(&[250, 0, 0, 255]);
        assert_ne!(axis_samples(&changed, CaptureAxis::Vertical), vertical);
        assert_ne!(axis_samples(&changed, CaptureAxis::Horizontal), horizontal);
    }

    #[test]
    fn axis_switch_is_locked_after_the_first_append() {
        let page = page(64, 240, 3);
        let mut stitcher = Stitcher::new(
            viewport(&page, 0, 100),
            CaptureAxis::Vertical,
            stitch_cap_height(64),
        );
        assert!(stitcher.set_axis(CaptureAxis::Horizontal));
        assert_eq!(stitcher.axis(), CaptureAxis::Horizontal);
        assert!(stitcher.set_axis(CaptureAxis::Vertical));
        assert_eq!(
            stitcher.tick(viewport(&page, 20, 100)),
            ScrollTick::Appended { fast: false }
        );
        assert!(!stitcher.set_axis(CaptureAxis::Horizontal));
        assert_eq!(stitcher.axis(), CaptureAxis::Vertical);
    }

    #[test]
    fn axis_switch_gate_requires_a_running_session_not_started() {
        assert!(axis_switch_allowed(STATE_RUNNING, false));
        assert!(!axis_switch_allowed(STATE_RUNNING, true));
        assert!(!axis_switch_allowed(STATE_FINISH, false));
        assert!(!axis_switch_allowed(STATE_IDLE, false));
    }

    /// 就绪态门:未显式开始时(按钮/滚轮/内容变化兜底都未到)自动 nudge
    /// 一律不发,宽限期与就绪信号都够久也一样。
    #[test]
    fn auto_scroll_stays_closed_until_the_session_is_started() {
        assert!(!auto_scroll_start_allowed(
            false,
            true,
            Duration::from_secs(60),
            Some(Duration::from_secs(60))
        ));
        assert!(!auto_scroll_start_allowed(
            false,
            false,
            AUTO_SCROLL_READY_TIMEOUT,
            None
        ));
    }

    /// P1 回归:自动滚动的首个 nudge 不得在控制窗可交互前发生,否则首个
    /// append 会把轴向锁死为纵向,横向不可达。
    #[test]
    fn auto_scroll_waits_for_the_control_window_and_a_settled_direction() {
        // 会话已开始但前端就绪信号未到:不滚动(旧实现 400ms 就发首个 nudge)。
        assert!(!auto_scroll_start_allowed(
            true,
            false,
            Duration::from_millis(400),
            None
        ));
        assert!(!auto_scroll_start_allowed(
            true,
            false,
            Duration::from_millis(1_000),
            None
        ));
        // 就绪信号缺失时,兜底时限前同样不滚动。
        assert!(!auto_scroll_start_allowed(
            true,
            false,
            AUTO_SCROLL_READY_TIMEOUT - Duration::from_millis(1),
            None
        ));
        // 控制窗已就绪但宽限期未到:留给用户选横向。
        assert!(!auto_scroll_start_allowed(
            true,
            false,
            Duration::from_secs(1),
            Some(Duration::ZERO)
        ));
        assert!(!auto_scroll_start_allowed(
            true,
            false,
            Duration::from_secs(3),
            Some(AUTO_SCROLL_GRACE - Duration::from_millis(1))
        ));
    }

    /// 方向选择落定后放行:用户点选立即开闸;不点选则宽限到期后仍按当前
    /// (默认纵向)方向开始,横向可达且纵向默认行为不变。
    #[test]
    fn auto_scroll_starts_after_a_direction_choice_or_the_grace_period() {
        assert!(auto_scroll_start_allowed(
            true,
            true,
            Duration::ZERO,
            Some(Duration::ZERO)
        ));
        assert!(auto_scroll_start_allowed(true, true, Duration::ZERO, None));
        assert!(auto_scroll_start_allowed(
            true,
            false,
            Duration::from_secs(10),
            Some(AUTO_SCROLL_GRACE)
        ));
        // 前端就绪信号迟迟不到:兜底时限后开始,避免自动滚动永久停摆。
        assert!(auto_scroll_start_allowed(
            true,
            false,
            AUTO_SCROLL_READY_TIMEOUT,
            None
        ));
    }

    /// 方向选择门:自动滚动开闸前选择横向必须真的走横向拼接(宽度增长),
    /// 之后才被首个 append 锁定。
    #[test]
    fn choosing_horizontal_before_the_first_append_stitches_columns() {
        let page = wide_page(300, 96, 5);
        let mut stitcher = Stitcher::new(
            horizontal_viewport(&page, 0, 120),
            CaptureAxis::Vertical,
            4000,
        );
        assert!(stitcher.set_axis(CaptureAxis::Horizontal));
        assert_eq!(stitcher.axis(), CaptureAxis::Horizontal);
        assert_eq!(
            stitcher.tick(horizontal_viewport(&page, 30, 120)),
            ScrollTick::Appended { fast: false }
        );
        assert_eq!(stitcher.width(), 150);
        assert_eq!(stitcher.height(), 96);
        assert_eq!(stitcher.appended(), 30);
    }

    /// 控制窗方向入口在显式开始前登记请求并标记「用户已选定」:
    /// 自动滚动开闸条件因此立即满足;开始后同一入口拒绝且不再标记。
    #[test]
    fn set_scroll_axis_before_start_marks_the_direction_chosen() {
        let previous_state = SCROLL_STATE.swap(STATE_RUNNING, Ordering::SeqCst);
        let previous_started = SCROLL_SESSION_STARTED.swap(false, Ordering::SeqCst);
        let mut status = LAST_STATUS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_status = status.take();
        *status = Some(ScrollStatus::new(
            "ready",
            CaptureAxis::Vertical,
            10,
            10,
            0,
            false,
            1,
        ));
        drop(status);
        SCROLL_AXIS_REQUEST.store(CaptureAxis::Vertical.as_u8(), Ordering::SeqCst);
        SCROLL_AXIS_CHOSEN.store(false, Ordering::SeqCst);
        assert!(set_scroll_axis(CaptureAxis::Horizontal).is_ok());
        assert!(SCROLL_AXIS_CHOSEN.load(Ordering::SeqCst));
        assert_eq!(
            CaptureAxis::from_u8(SCROLL_AXIS_REQUEST.load(Ordering::SeqCst)),
            CaptureAxis::Horizontal
        );
        // 已显式开始:拒绝切换,也不把方向标成已选定(锁定语义不变)。
        SCROLL_SESSION_STARTED.store(true, Ordering::SeqCst);
        SCROLL_AXIS_CHOSEN.store(false, Ordering::SeqCst);
        let mut status = LAST_STATUS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *status = Some(ScrollStatus::new(
            "running",
            CaptureAxis::Horizontal,
            10,
            10,
            0,
            false,
            1,
        ));
        drop(status);
        assert!(set_scroll_axis(CaptureAxis::Vertical).is_err());
        assert!(!SCROLL_AXIS_CHOSEN.load(Ordering::SeqCst));
        // 恢复全局状态,避免影响其他测试。
        let mut status = LAST_STATUS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *status = previous_status;
        drop(status);
        SCROLL_STATE.store(previous_state, Ordering::SeqCst);
        SCROLL_SESSION_STARTED.store(previous_started, Ordering::SeqCst);
    }

    /// R2:追加一段后可回退——长度、appended、锚帧同步回拨,首帧不可回退;
    /// 回退后继续滚动按恢复的锚帧正常续接,不重复不错位。
    #[test]
    fn undo_last_segment_rewinds_the_stitch_and_resumes_cleanly() {
        let page = page(72, 400, 11);
        let mut stitcher = Stitcher::new(viewport(&page, 0, 100), CaptureAxis::Vertical, 1000);
        // 首帧不可回退(栈空)。
        assert!(!stitcher.undo_last_segment());
        assert!(!stitcher.can_undo());
        let mut top = 0u32;
        for delta in [20u32, 7, 33] {
            top += delta;
            assert_eq!(
                stitcher.tick(viewport(&page, top, 100)),
                ScrollTick::Appended { fast: false }
            );
        }
        assert!(stitcher.can_undo());
        assert_eq!(stitcher.height(), 100 + 60);
        assert_eq!(stitcher.appended(), 60);
        // 回退最后一段(33px):锚帧恢复到该段追加前的帧。
        assert!(stitcher.undo_last_segment());
        assert_eq!(stitcher.height(), 100 + 27);
        assert_eq!(stitcher.appended(), 27);
        // 从 top-33 之后的位置续接:新帧与恢复的锚帧之间位移按真实滚动估计。
        let resume = top - 33 + 12;
        assert_eq!(
            stitcher.tick(viewport(&page, resume, 100)),
            ScrollTick::Appended { fast: false }
        );
        let stitched = stitcher.into_frame();
        let expected = crop_rgba(&page, 0, 0, 72, 100 + resume).unwrap();
        assert_eq!(stitched.rgba, expected.rgba);
    }

    /// R2:连续回退到只剩首帧后栈空,再回退返回 false 且状态不变。
    #[test]
    fn undo_drains_the_segment_stack_then_stops() {
        let page = page(64, 300, 3);
        let mut stitcher = Stitcher::new(viewport(&page, 0, 100), CaptureAxis::Vertical, 1000);
        for top in [20u32, 40] {
            stitcher.tick(viewport(&page, top, 100));
        }
        assert!(stitcher.undo_last_segment());
        assert!(stitcher.undo_last_segment());
        assert_eq!(stitcher.height(), 100);
        assert_eq!(stitcher.appended(), 0);
        // scrolled 语义不因回退改变(已有过滚动内容);can_undo 为假。
        assert!(stitcher.scrolled());
        assert!(!stitcher.can_undo());
        assert!(!stitcher.undo_last_segment());
        assert_eq!(stitcher.height(), 100);
    }

    /// R2:上限触顶(limit_reached)后回退一段,上限状态与 appended 同步回拨。
    #[test]
    fn undo_after_the_cap_rewinds_limit_reached() {
        let page = page(48, 400, 9);
        let cap = 100 + 30;
        let mut stitcher =
            Stitcher::new(viewport(&page, 0, 100), CaptureAxis::Vertical, cap);
        assert_eq!(
            stitcher.tick(viewport(&page, 50, 100)),
            ScrollTick::LimitReached
        );
        assert!(stitcher.limit_reached());
        assert_eq!(stitcher.appended(), 30);
        assert!(stitcher.undo_last_segment());
        assert_eq!(stitcher.height(), 100);
        assert_eq!(stitcher.appended(), 0);
        assert!(!stitcher.limit_reached());
    }

    /// R2:RESYNC 换锚本身不入栈,也不破坏既有段栈:追加若干段后换锚,
    /// 逐段回退仍把 prev 恢复到各段追加前的锚帧,且尺寸/appended 逐步回拨。
    /// (换锚后能否再追加取决于新锚内容,不影响本契约:段锚随 append 记录。)
    #[test]
    fn undo_restores_the_anchor_before_a_resync() {
        let page_a = page(64, 260, 0);
        let mut stitcher =
            Stitcher::new(viewport(&page_a, 0, 120), CaptureAxis::Vertical, 5000);
        // 两段纹理追加,各自的段锚为 prev(初帧、追加一后的帧)。
        assert_eq!(
            stitcher.tick(viewport(&page_a, 20, 120)),
            ScrollTick::Appended { fast: false }
        );
        assert_eq!(
            stitcher.tick(viewport(&page_a, 35, 120)),
            ScrollTick::Appended { fast: false }
        );
        // RESYNC 换锚:连续对不齐,prev 换成无关帧。换锚本身不入栈。
        let resynced = flat(64, 120, 200);
        for _ in 0..RESYNC_AFTER {
            assert_eq!(stitcher.tick(resynced.clone()), ScrollTick::NoMatch);
        }
        assert_eq!(stitcher.anchor().rgba, resynced.rgba);
        // 回退末段:prev 恢复为该段追加前的锚(page_a@20 帧),而不是换锚后的
        // flat 帧——段锚在追加时快照,与之后的换锚无关。
        assert!(stitcher.undo_last_segment());
        assert_eq!(stitcher.anchor().rgba, viewport(&page_a, 20, 120).rgba);
        assert_eq!(stitcher.height(), 140);
        assert_eq!(stitcher.appended(), 20);
        // 再退一段:恢复到初始帧,栈空。
        assert!(stitcher.undo_last_segment());
        assert_eq!(stitcher.anchor().rgba, viewport(&page_a, 0, 120).rgba);
        assert_eq!(stitcher.height(), 120);
        assert!(!stitcher.can_undo());
    }

    /// R2:横向追加段同样可回退——宽度与 appended 回拨,像素逐字节一致。
    #[test]
    fn horizontal_undo_last_segment_rewinds_columns() {
        let page = wide_page(400, 72, 11);
        let mut stitcher = Stitcher::new(
            horizontal_viewport(&page, 0, 100),
            CaptureAxis::Horizontal,
            1000,
        );
        for left in [20u32, 40] {
            stitcher.tick(horizontal_viewport(&page, left, 100));
        }
        assert_eq!(stitcher.width(), 140);
        assert!(stitcher.undo_last_segment());
        assert_eq!(stitcher.width(), 120);
        assert_eq!(stitcher.appended(), 20);
        assert!(stitcher.undo_last_segment());
        assert_eq!(stitcher.width(), 100);
        assert_eq!(stitcher.appended(), 0);
        assert!(!stitcher.can_undo());
    }

    /// R2:回退命令门:仅 RUNNING(且未进入完成流程)接受,其他状态拒绝;
    /// 栈空语义下首个内容变化前调用也是 no-op 而不报错(门只看会话状态)。
    #[test]
    fn undo_scroll_segment_requires_a_running_session() {
        let previous_state = SCROLL_STATE.swap(STATE_IDLE, Ordering::SeqCst);
        assert!(undo_scroll_segment().is_err());
        SCROLL_STATE.store(STATE_FINISH, Ordering::SeqCst);
        assert!(undo_scroll_segment().is_err());
        SCROLL_STATE.store(STATE_FINISHING, Ordering::SeqCst);
        assert!(undo_scroll_segment().is_err());
        SCROLL_STATE.store(STATE_RUNNING, Ordering::SeqCst);
        assert!(undo_scroll_segment().is_ok());
        SCROLL_STATE.store(previous_state, Ordering::SeqCst);
    }

    #[test]
    fn stitch_cap_shrinks_with_the_cross_axis_and_never_reaches_zero() {
        assert_eq!(stitch_cap_height(100), MAX_STITCH_LENGTH);
        assert_eq!(stitch_cap_width(100), MAX_STITCH_LENGTH);
        assert_eq!(
            stitch_cap_height(u32::MAX),
            (MAX_STITCH_PIXELS / u64::from(u32::MAX)).max(1) as u32
        );
        assert_eq!(
            stitch_cap_width(u32::MAX),
            (MAX_STITCH_PIXELS / u64::from(u32::MAX)).max(1) as u32
        );
        assert!(stitch_cap_height(0) > 0);
        assert!(stitch_cap_width(0) > 0);
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
    fn strip_length_caps_and_never_collapses() {
        assert_eq!(strip_length(1000), STRIP_LENGTH);
        assert_eq!(strip_length(90), 30);
        assert_eq!(strip_length(0), 1);
    }

    #[test]
    fn scroll_status_serializes_for_the_control_window() {
        let json = serde_json::to_value(ScrollStatus::new(
            "finishing",
            CaptureAxis::Horizontal,
            640,
            1200,
            90,
            true,
            7,
        ))
        .unwrap();
        assert_eq!(json["state"], "finishing");
        assert_eq!(json["axis"], "horizontal");
        assert_eq!(json["width"], 640);
        assert_eq!(json["height"], 1200);
        assert_eq!(json["appended"], 90);
        assert_eq!(json["canUndo"], true);
        // R7:finishing 的「正在拼接 · N 段」消费 camelCase 段数字段。
        assert_eq!(json["segmentCount"], 7);
    }

    /// R7:段数随追加增长、随回退扣减;超过回退栈上限后仍如实计数
    /// (栈会丢最旧段,但拼接结果的构成段数不受影响)。
    #[test]
    fn segment_count_tracks_appends_and_undos_beyond_the_undo_stack_cap() {
        let step = 20u32;
        let total_appends = MAX_UNDO_SEGMENTS as u32 + 6;
        let page_height = 120 + step * total_appends;
        let page = page(64, page_height, 5);
        let mut stitcher =
            Stitcher::new(viewport(&page, 0, 120), CaptureAxis::Vertical, page_height * 2);

        assert_eq!(stitcher.segment_count(), 1);
        for tick in 1..=total_appends {
            let top = step * tick;
            assert_eq!(
                stitcher.tick(viewport(&page, top, 120)),
                ScrollTick::Appended { fast: false }
            );
            assert_eq!(stitcher.segment_count(), 1 + tick);
        }
        // 栈已封顶丢弃最旧段,段数仍等于初始区域 + 全部追加。
        assert_eq!(stitcher.segment_count(), 1 + total_appends);

        // 回退只回拨最近一段:段数随之 -1,像素与高度同步回退。
        let height_before = stitcher.height();
        assert!(stitcher.undo_last_segment());
        assert_eq!(stitcher.segment_count(), total_appends);
        assert_eq!(stitcher.height(), height_before - step);
        // 回退到栈空:可回退段(上限 64)全部撤销;被栈丢弃的最旧 6 段仍在
        // 图里,所以段数 = 初始区域 + 未回退的追加段,高度同理。
        while stitcher.can_undo() {
            assert!(stitcher.undo_last_segment());
        }
        let kept = total_appends - MAX_UNDO_SEGMENTS as u32;
        assert_eq!(stitcher.segment_count(), 1 + kept);
        assert!(!stitcher.can_undo());
        assert_eq!(stitcher.height(), 120 + kept * step);
    }

    #[test]
    fn set_scroll_axis_arg_accepts_both_directions() {
        // Tauri 命令参数按 serde 反序列化:前端用 "vertical"/"horizontal"。
        assert_eq!(
            serde_json::from_str::<CaptureAxis>("\"vertical\"").unwrap(),
            CaptureAxis::Vertical
        );
        assert_eq!(
            serde_json::from_str::<CaptureAxis>("\"horizontal\"").unwrap(),
            CaptureAxis::Horizontal
        );
        assert_eq!(CaptureAxis::default(), CaptureAxis::Vertical);
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

            let mut stitcher = Stitcher::new(grab_window(), CaptureAxis::Vertical, 4000);
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
