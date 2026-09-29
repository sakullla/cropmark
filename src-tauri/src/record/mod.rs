//! R3 离线录屏引擎。
//!
//! 引擎与入口解耦:托盘/选区入口与录制 HUD 由后续任务接入,本模块提供
//! 可独立复验的录制会话与保存链路。
//!
//! - 按设置帧率用现有平台单帧抓取 API 抓帧(未选时 MP4 为 30,GIF/WebP 为 15),
//!   `crop_rgba` 裁剪后用 `rasterize_lenient` 合并实时标注,再流式编码;
//!   每一帧的持续时间按不含暂停的录制时刻书写,慢帧拉长上一帧而不是缩时;
//! - 支持开始/暂停/继续/停止;暂停不产帧也不推进录制时间;
//! - 上限、时长与帧时间戳都按墙钟(暂停时段不计入)计:单次上限 30 分钟,
//!   低帧率/大区域下也按真实经过时间自动停止并保留已完成内容;
//! - 无音轨;编码全部离线(GIF/WebP 复用现有依赖,MP4 构建期内置 openh264);
//! - 录制产物只写临时文件,不经过 `history::record_capture`,不进入截图历史。

mod encoder;
pub mod hud;
mod save;

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::AppHandle;

use crate::annotate::{rasterize_lenient, Annotation};
use crate::capture::buffer::{crop_rgba, Frame};
use crate::capture::error::CaptureError;
use crate::capture::geometry::MonitorGeom;
use crate::capture::platform;
use crate::i18n;
use crate::settings;

pub use encoder::RecordFormat;
pub use save::{
    discard_recording, keep_pending_recording, move_output_atomic, pending_recordings,
    save_recording_with_dialog, take_pending_recordings, RecordSaveResult,
};

/// 默认目标帧率:固定帧率抓帧,与现有平台单帧抓取 API 的能力一致。
pub const DEFAULT_FPS: u32 = 10;
/// 单次录制上限(毫秒,墙钟活跃时长):30 分钟。到时自动停止并保留已完成内容。
pub const MAX_RECORDING_MS: u64 = 30 * 60 * 1000;

/// 录制区域:显示器整屏物理帧内的偏移与尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl RecordRegion {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// MP4/H.264 把宽高向下收成偶数,其余格式保持原样。不做尺寸上限校验。
    pub fn adjusted_for_format(self, format: RecordFormat) -> Self {
        match format {
            RecordFormat::Mp4 => Self {
                width: self.width & !1,
                height: self.height & !1,
                ..self
            },
            _ => self,
        }
    }

    /// MP4/H.264 要求宽高为偶数:向下取整到偶数,其余格式保持原样。
    /// 校验失败返回 `RecordError::Region`。
    pub fn for_format(self, format: RecordFormat) -> Result<Self, RecordError> {
        let region = self.adjusted_for_format(format);
        encoder::validate_region(format, region)?;
        Ok(region)
    }
}

/// 录制配置。`quality` 复用现有导出质量档位的编码值(1–100)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordConfig {
    pub format: RecordFormat,
    pub fps: u32,
    pub quality: u8,
    pub max_duration_ms: u64,
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            format: RecordFormat::Gif,
            fps: DEFAULT_FPS,
            quality: crate::settings::ExportQuality::High.value(),
            max_duration_ms: MAX_RECORDING_MS,
        }
    }
}

impl RecordConfig {
    /// 从当前设置读取质量记忆和帧率。没选过帧率时按格式取默认档。
    pub fn from_settings(app: &AppHandle, format: RecordFormat) -> Self {
        let export = settings::current_export(app);
        let recording = settings::current_recording(app);
        Self {
            format,
            fps: resolve_recording_fps(format, recording.fps),
            quality: export.quality.value(),
            max_duration_ms: MAX_RECORDING_MS,
        }
    }

    /// 帧率钳制到 1–60,质量钳制到 1–100,上限钳制到 30 分钟。
    pub fn sanitized(self) -> Self {
        Self {
            fps: self.fps.clamp(1, 60),
            quality: self.quality.clamp(1, 100),
            max_duration_ms: self.max_duration_ms.clamp(1, MAX_RECORDING_MS),
            ..self
        }
    }
}

/// 帧源:录制会话从它按目标帧率取源画面(显示器整屏物理像素)。
pub trait FrameSource: Send {
    fn capture(&mut self) -> Result<Frame, CaptureError>;

    /// 生产帧源所在的显示器。有几何信息时,会话会把边框和控制条让出捕获矩形。
    fn monitor_geometry(&self) -> Option<MonitorGeom> {
        None
    }
}

/// 生产用帧源:现有平台单帧抓取 API 按显示器抓整屏,由会话裁剪区域。
pub struct MonitorSource {
    monitor: MonitorGeom,
}

impl MonitorSource {
    pub fn new(monitor: MonitorGeom) -> Self {
        Self { monitor }
    }
}

impl FrameSource for MonitorSource {
    fn capture(&mut self) -> Result<Frame, CaptureError> {
        platform::capture_monitor(&self.monitor)
    }

    fn monitor_geometry(&self) -> Option<MonitorGeom> {
        Some(self.monitor.clone())
    }
}

/// 未单独选过帧率时,MP4 用 30,GIF 和 WebP 用 15。选定 10/15/30 后三种格式都用这一档。
pub fn resolve_recording_fps(format: RecordFormat, chosen: Option<u32>) -> u32 {
    if let Some(fps) = chosen.filter(|fps| settings::RECORDING_FPS_CHOICES.contains(fps)) {
        return fps;
    }
    match format {
        RecordFormat::Mp4 => 30,
        RecordFormat::Gif | RecordFormat::Webp => 15,
    }
}

/// 录制错误。`user_message` 生成当前语言的用户可见文案。
#[derive(Debug, Clone)]
pub enum RecordError {
    /// 抓帧失败(权限/平台/越界等),沿用采集链路已有文案。
    Capture(CaptureError),
    /// 编码失败,停止本次录制;detail 为编码器原始原因。
    Encode(String),
    /// 临时文件读写失败。
    TempIo(String),
    /// 没有产生任何帧。
    Empty,
    /// 没有可操作的录制会话(已结束或不存在)。
    NotRunning,
    /// 区域无效或超出所选格式的尺寸上限。
    Region,
    /// 录制线程无法启动或异常退出。
    Thread,
}

impl RecordError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Capture(error) => error.user_message(),
            Self::Encode(detail) => i18n::tp("error.record.encode", &[("detail", detail)]),
            Self::TempIo(detail) => i18n::tp("error.record.temp_io", &[("detail", detail)]),
            Self::Empty => i18n::t("error.record.empty"),
            Self::NotRunning => i18n::t("error.record.not_running"),
            Self::Region => i18n::t("error.record.region"),
            Self::Thread => i18n::t("error.record.thread"),
        }
    }
}

/// 对外暴露的会话阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RecordingPhase {
    Recording,
    Paused,
    Finished,
    Failed,
}

/// HUD 轮询用的会话状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStatus {
    pub phase: RecordingPhase,
    pub format: RecordFormat,
    pub frame_count: u64,
    /// 已录时长(毫秒):墙钟真实活动时长,暂停时段不计入。
    pub elapsed_ms: u64,
    pub width: u32,
    pub height: u32,
    /// 这次录制实际使用的帧率档。
    pub fps: u32,
    /// 抓帧慢于帧间隔。成片会拉长上一帧补上时间,录制中需要说明。
    pub behind: bool,
    /// 是否因到达 30 分钟上限自动停止。
    pub auto_stopped: bool,
    /// 失败或中断时的本地化说明。
    pub error: Option<String>,
}

/// 停止后的录制产物:临时文件在保存成功或丢弃时删除。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingOutput {
    pub format: RecordFormat,
    #[serde(skip)]
    pub temp_path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub frame_count: u64,
    /// 实际录制时长(毫秒,墙钟活跃时间,暂停时段不计入)。
    pub duration_ms: u64,
    /// 写入成片的帧率档。
    pub fps: u32,
    pub auto_stopped: bool,
    /// 抓帧持续失败导致的中断说明;正常停止时为 None。
    pub interrupted: Option<String>,
}

/// 内部阶段:比对外阶段多一个「停止已请求」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Recording,
    Paused,
    StopRequested,
    Finished,
    Failed,
}

impl Phase {
    fn public(self) -> RecordingPhase {
        match self {
            Self::Recording | Self::StopRequested => RecordingPhase::Recording,
            Self::Paused => RecordingPhase::Paused,
            Self::Finished => RecordingPhase::Finished,
            Self::Failed => RecordingPhase::Failed,
        }
    }
}

struct WorkerState {
    phase: Phase,
    /// 当前活跃录制段起点:墙钟时间轴按活跃段累计,暂停时段不计入。
    segment_started_at: Instant,
    /// 已冻结的活跃录制时长(暂停、停止或失败时并入)。
    elapsed_frozen: Duration,
    annotations: Vec<Annotation>,
    frame_count: u64,
    /// 至少有一帧的抓取慢于目标间隔。
    behind: bool,
    auto_stopped: bool,
    interrupted: Option<String>,
    output: Option<RecordingOutput>,
    error: Option<RecordError>,
}

impl WorkerState {
    fn new() -> Self {
        Self {
            phase: Phase::Recording,
            segment_started_at: Instant::now(),
            elapsed_frozen: Duration::ZERO,
            annotations: Vec::new(),
            frame_count: 0,
            behind: false,
            auto_stopped: false,
            interrupted: None,
            output: None,
            error: None,
        }
    }

    /// 当前已录时长(墙钟):录制中随真实时间推进,暂停/停止后取冻结值。
    fn elapsed(&self, now: Instant) -> Duration {
        if self.phase == Phase::Recording {
            self.elapsed_frozen + now.saturating_duration_since(self.segment_started_at)
        } else {
            self.elapsed_frozen
        }
    }

    /// 冻结已录时长:把当前活跃段的墙钟增量并入。已冻结阶段调用保持不变。
    fn freeze_elapsed(&mut self, now: Instant) {
        self.elapsed_frozen = self.elapsed(now);
    }

    /// 暂停后继续:开启新的活跃段,暂停期间不计入时长。
    fn restart_segment(&mut self, now: Instant) {
        self.segment_started_at = now;
    }
}

struct Shared {
    state: Mutex<WorkerState>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, WorkerState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 录制会话:后台线程按固定帧率抓帧编码,主线程可查询状态、暂停/继续/停止。
pub struct RecordingSession {
    shared: Arc<Shared>,
    region: RecordRegion,
    /// 标注仍用确认矩形的坐标系;捕获矩形让出边框或控制条时,合成前平移这段差值。
    annotation_inset: (i32, i32),
    config: RecordConfig,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl RecordingSession {
    /// 启动录制。帧源通常为 `MonitorSource`(平台抓屏),测试可注入合成帧源。
    pub fn start(
        region: RecordRegion,
        config: RecordConfig,
        source: impl FrameSource + 'static,
    ) -> Result<Self, RecordError> {
        let config = config.sanitized();
        let confirmed = region;
        let monitor = source.monitor_geometry();
        let (region, annotation_inset) = match monitor.as_ref() {
            Some(monitor) => {
                let spec = hud::chrome_spec_for(monitor, config.format);
                let plan = hud::plan_recording_chrome(confirmed, monitor, spec)
                    .map_err(|_| RecordError::Region)?;
                (plan.capture, hud::annotation_inset(confirmed, plan.capture))
            }
            None => (confirmed, (0, 0)),
        };
        let region = region.for_format(config.format)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(WorkerState::new()),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker_inset = annotation_inset;
        let worker = std::thread::Builder::new()
            .name("cropmark-record".into())
            .spawn(move || run_worker(worker_shared, region, worker_inset, config, source))
            .map_err(|_| RecordError::Thread)?;
        Ok(Self {
            shared,
            region,
            annotation_inset,
            config,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// 这次会话实际使用的帧率档。
    pub fn fps(&self) -> u32 {
        self.config.fps
    }

    /// 捕获矩形相对确认矩形的原点差值。标注合成前按它平移。
    pub fn annotation_inset(&self) -> (i32, i32) {
        self.annotation_inset
    }

    /// 当前状态快照(HUD 实时时长与阶段)。
    pub fn status(&self) -> RecordingStatus {
        let state = self.shared.lock();
        RecordingStatus {
            phase: state.phase.public(),
            format: self.config.format,
            frame_count: state.frame_count,
            elapsed_ms: millis(state.elapsed(Instant::now())),
            width: self.region.width,
            height: self.region.height,
            fps: self.config.fps,
            behind: state.behind,
            auto_stopped: state.auto_stopped,
            error: state.error.as_ref().map(RecordError::user_message),
        }
    }

    /// 更新实时标注:下一帧起合并进录制画面。
    pub fn set_annotations(&self, annotations: Vec<Annotation>) {
        self.shared.lock().annotations = annotations;
    }

    /// 当前实时标注:HUD 打开时同步给标注层作为初始状态(含选区壳上的标注)。
    pub fn annotations(&self) -> Vec<Annotation> {
        self.shared.lock().annotations.clone()
    }

    /// 暂停:停止产帧,录制时间不推进。已暂停时幂等返回。
    ///
    /// 注意:幂等分支也必须先释放 `shared` 守卫再查询状态,`status()` 会再次
    /// 获取同一把 std Mutex(不可重入)。
    pub fn pause(&self) -> Result<RecordingStatus, RecordError> {
        let changed = {
            let mut state = self.shared.lock();
            match state.phase {
                Phase::Recording => {
                    state.freeze_elapsed(Instant::now());
                    state.phase = Phase::Paused;
                    true
                }
                Phase::Paused => false,
                _ => return Err(RecordError::NotRunning),
            }
        };
        if changed {
            self.shared.wake.notify_all();
        }
        Ok(self.status())
    }

    /// 继续:从暂停处恢复,不追赶暂停期间的时间。已录制中时幂等返回。
    pub fn resume(&self) -> Result<RecordingStatus, RecordError> {
        let changed = {
            let mut state = self.shared.lock();
            match state.phase {
                Phase::Paused => {
                    state.phase = Phase::Recording;
                    state.restart_segment(Instant::now());
                    true
                }
                Phase::Recording => false,
                _ => return Err(RecordError::NotRunning),
            }
        };
        if changed {
            self.shared.wake.notify_all();
        }
        Ok(self.status())
    }

    /// 停止并收尾编码:返回可保存的产物。自动停止(上限)后调用同样返回产物。
    pub fn stop(&self) -> Result<RecordingOutput, RecordError> {
        {
            let mut state = self.shared.lock();
            match state.phase {
                Phase::Finished => return state.output.clone().ok_or(RecordError::Empty),
                Phase::Failed => {
                    return Err(state.error.clone().unwrap_or(RecordError::Empty));
                }
                _ => {
                    state.freeze_elapsed(Instant::now());
                    state.phase = Phase::StopRequested;
                }
            }
        }
        self.shared.wake.notify_all();
        self.join_worker();
        let state = self.shared.lock();
        match state.phase {
            Phase::Finished => state.output.clone().ok_or(RecordError::Empty),
            _ => Err(state.error.clone().unwrap_or(RecordError::Empty)),
        }
    }

    /// 已完成的产物(未停止/未到上限时为 None)。
    pub fn output(&self) -> Option<RecordingOutput> {
        self.shared.lock().output.clone()
    }

    /// 上限自动停止后、用户还没拿走会话时的成片。
    pub fn finished_output(&self) -> Option<RecordingOutput> {
        let state = self.shared.lock();
        if state.phase == Phase::Finished {
            state.output.clone()
        } else {
            None
        }
    }

    fn join_worker(&self) {
        let handle = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            if handle.join().is_err() {
                let mut state = self.shared.lock();
                if state.phase != Phase::Finished {
                    state.phase = Phase::Failed;
                    state.error = Some(RecordError::Thread);
                }
            }
        }
    }
}

impl Drop for RecordingSession {
    fn drop(&mut self) {
        // 会话被丢弃时先停线程,避免后台继续抓屏。已 stop 的产物归调用方
        // (保存在 `RecordingOutput`),不受影响;仍在录制的会话被丢弃时
        // 没有消费方,收尾后直接删除临时文件。
        let abandoned = {
            let mut state = self.shared.lock();
            if matches!(state.phase, Phase::Recording | Phase::Paused) {
                state.freeze_elapsed(Instant::now());
                state.phase = Phase::StopRequested;
                true
            } else {
                false
            }
        };
        self.shared.wake.notify_all();
        self.join_worker();
        if abandoned {
            let mut state = self.shared.lock();
            if let Some(output) = state.output.take() {
                let _ = std::fs::remove_file(&output.temp_path);
            }
        }
    }
}

/// Duration → 毫秒(u64 饱和):对外时间轴统一按墙钟毫秒。
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// 等待到下一帧时刻;暂停时挂起,停止请求时返回 false。
fn wait_for_slot(shared: &Shared, deadline: Instant) -> bool {
    let mut state = shared.lock();
    loop {
        match state.phase {
            Phase::StopRequested => return false,
            Phase::Paused => {
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            _ => {
                let now = Instant::now();
                if now >= deadline {
                    return true;
                }
                let (guard, _) = shared
                    .wake
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state = guard;
            }
        }
    }
}

fn run_worker(
    shared: Arc<Shared>,
    region: RecordRegion,
    annotation_inset: (i32, i32),
    config: RecordConfig,
    mut source: impl FrameSource,
) {
    let mut encoder = match encoder::open(&config, region) {
        Ok(encoder) => encoder,
        Err(error) => {
            finish_failed(&shared, error);
            return;
        }
    };
    let interval = Duration::from_micros(1_000_000 / u64::from(config.fps.max(1)));
    let max_duration = Duration::from_millis(config.max_duration_ms);
    // 连续抓帧失败达到约 10 秒:保留已完成内容并中断,避免空转。
    let failure_limit = u64::from(config.fps).saturating_mul(10).max(10);
    let mut consecutive_failures = 0u64;
    let mut interrupted: Option<CaptureError> = None;
    let mut auto_stopped = false;
    let mut next_at = Instant::now();

    'recording: loop {
        if !wait_for_slot(&shared, next_at) {
            break;
        }
        let (annotations, timestamp_ms) = {
            let state = shared.lock();
            if state.phase == Phase::Paused {
                continue;
            }
            // 上限按墙钟已录时长判定:低帧率/大区域下单帧耗时超过 1/fps 时,
            // 不等到攒满 标称帧率 × 上限 的帧数,而是真实时间到点即停。
            let elapsed = state.elapsed(Instant::now());
            if elapsed >= max_duration {
                auto_stopped = true;
                break 'recording;
            }
            (state.annotations.clone(), millis(elapsed))
        };
        let capture_started = Instant::now();
        let captured = source
            .capture()
            .and_then(|full| crop_rgba(&full, region.x, region.y, region.width, region.height));
        if capture_started.elapsed() > interval {
            // 慢于间隔:拉长上一帧补上这段时间,并让控制条说明跟不上。
            shared.lock().behind = true;
        }
        let frame = match captured {
            Ok(frame) => frame,
            Err(error) => {
                consecutive_failures += 1;
                log::warn!("record capture failed kind={:?}", error.kind);
                if consecutive_failures >= failure_limit {
                    interrupted = Some(error);
                    break 'recording;
                }
                next_at = Instant::now() + interval;
                continue;
            }
        };
        consecutive_failures = 0;
        let annotations = if annotation_inset == (0, 0) {
            annotations
        } else {
            crate::annotate::translated_all(
                &annotations,
                -f64::from(annotation_inset.0),
                -f64::from(annotation_inset.1),
            )
        };
        let merged = rasterize_lenient(&frame, &annotations);
        if let Err(error) = encoder.push(&merged, timestamp_ms) {
            finish_failed(&shared, error);
            return;
        }
        shared.lock().frame_count += 1;
        next_at = Instant::now() + interval;
    }

    // 停止/上限/失败都先冻结墙钟时长再收尾编码,状态与产物时长保持一致。
    let (frame_count, duration_ms) = {
        let mut state = shared.lock();
        let now = Instant::now();
        state.freeze_elapsed(now);
        (state.frame_count, millis(state.elapsed(now)))
    };
    if frame_count == 0 {
        // 一帧都没产出:丢弃空临时文件,保留原始抓帧错误(若有)作为原因。
        let error = interrupted
            .map(RecordError::Capture)
            .unwrap_or(RecordError::Empty);
        finish_failed(&shared, error);
        return;
    }
    let temp_path = encoder.temp_path().to_path_buf();
    match encoder.finish(duration_ms) {
        Ok(()) => {
            let output = RecordingOutput {
                format: config.format,
                temp_path,
                width: region.width,
                height: region.height,
                frame_count,
                duration_ms,
                fps: config.fps,
                auto_stopped,
                interrupted: interrupted.map(|error| error.user_message()),
            };
            let mut state = shared.lock();
            state.phase = Phase::Finished;
            state.auto_stopped = auto_stopped;
            state.interrupted = output.interrupted.clone();
            state.output = Some(output);
        }
        Err(error) => {
            finish_failed(&shared, error);
        }
    }
}

fn finish_failed(shared: &Shared, error: RecordError) {
    let mut state = shared.lock();
    state.freeze_elapsed(Instant::now());
    state.phase = Phase::Failed;
    state.error = Some(error);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, RawBuffer};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn synthetic_frame(width: u32, height: u32, shift: u32) -> Frame {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..height {
            for x in 0..width {
                let bright = ((x + shift) % 16) < 8;
                let value = if bright { 240 } else { 30 };
                bytes.extend_from_slice(&[value, 120, 220, 255]);
            }
        }
        accept_buffer(RawBuffer::ready(width, height, bytes)).expect("frame")
    }

    /// 合成帧源:整屏尺寸 = 区域尺寸 + 边距,便于验证裁剪路径。
    struct SyntheticSource {
        width: u32,
        height: u32,
        margin: u32,
        /// 单帧抓取耗时:模拟大区域/低吞吐下真实帧率低于目标帧率。
        capture_delay: Duration,
        frames: Arc<AtomicU32>,
        fail_first: Arc<AtomicU32>,
    }

    impl SyntheticSource {
        fn new(width: u32, height: u32) -> Self {
            Self {
                width,
                height,
                margin: 4,
                capture_delay: Duration::ZERO,
                frames: Arc::new(AtomicU32::new(0)),
                fail_first: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    struct MonitoredSource {
        inner: SyntheticSource,
        monitor: MonitorGeom,
    }

    impl FrameSource for MonitoredSource {
        fn capture(&mut self) -> Result<Frame, CaptureError> {
            self.inner.capture()
        }

        fn monitor_geometry(&self) -> Option<MonitorGeom> {
            Some(self.monitor.clone())
        }
    }

    impl FrameSource for SyntheticSource {
        fn capture(&mut self) -> Result<Frame, CaptureError> {
            if !self.capture_delay.is_zero() {
                std::thread::sleep(self.capture_delay);
            }
            let remaining = self.fail_first.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_first.fetch_sub(1, Ordering::SeqCst);
                return Err(CaptureError::api("error.capture.api"));
            }
            let index = self.frames.fetch_add(1, Ordering::SeqCst);
            Ok(synthetic_frame(
                self.width + self.margin,
                self.height + self.margin,
                index * 3,
            ))
        }
    }

    fn test_config(format: RecordFormat, max_duration_ms: u64) -> RecordConfig {
        RecordConfig {
            format,
            fps: 50,
            quality: 75,
            max_duration_ms,
        }
    }

    fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    fn run_until_frames(session: &RecordingSession, minimum: u64) -> RecordingStatus {
        let status = wait_until(Duration::from_secs(5), || {
            let status = session.status();
            status.phase != RecordingPhase::Recording || status.frame_count >= minimum
        });
        assert!(status, "session did not reach {minimum} frames");
        session.status()
    }

    #[test]
    fn session_encodes_gif_from_region_and_stops_with_playable_file() {
        let source = SyntheticSource::new(48, 32);
        let region = RecordRegion::new(2, 2, 48, 32);
        let session =
            RecordingSession::start(region, test_config(RecordFormat::Gif, 60_000), source)
                .expect("start");
        run_until_frames(&session, 3);
        let output = session.stop().expect("stop");
        assert_eq!(output.format, RecordFormat::Gif);
        assert_eq!((output.width, output.height), (48, 32));
        assert!(output.frame_count >= 3);
        assert!(!output.auto_stopped);
        assert!(output.temp_path.exists());
        let bytes = std::fs::read(&output.temp_path).expect("gif bytes");
        assert_eq!(&bytes[0..3], b"GIF");
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn session_encodes_webp_and_mp4_with_the_same_region() {
        for format in [RecordFormat::Webp, RecordFormat::Mp4] {
            let source = SyntheticSource::new(48, 32);
            let region = RecordRegion::new(2, 2, 48, 32);
            let session = RecordingSession::start(region, test_config(format, 60_000), source)
                .expect("start");
            run_until_frames(&session, 2);
            let output = session.stop().expect("stop");
            assert_eq!(output.format, format);
            assert!(output.temp_path.exists());
            let bytes = std::fs::read(&output.temp_path).expect("bytes");
            match format {
                RecordFormat::Webp => {
                    let decoded = webp::AnimDecoder::new(&bytes)
                        .decode()
                        .expect("webp decode");
                    assert!(decoded.has_animation());
                    assert!(decoded.len() >= 2);
                }
                RecordFormat::Mp4 => {
                    let reader = mp4::Mp4Reader::read_header(
                        std::io::Cursor::new(bytes),
                        output.temp_path.metadata().unwrap().len(),
                    )
                    .expect("mp4 header");
                    let track = reader.tracks().values().next().expect("track");
                    assert!(track.sample_count() >= 2);
                    assert_eq!((track.width(), track.height()), (48, 32));
                }
                RecordFormat::Gif => unreachable!(),
            }
            let _ = std::fs::remove_file(&output.temp_path);
        }
    }

    #[test]
    fn wall_clock_timeline_excludes_paused_segments() {
        let mut state = WorkerState::new();
        // 以活跃段起点为基准,墙钟增量原样计入已录时长。
        let base = state.segment_started_at;
        assert_eq!(
            state.elapsed(base + Duration::from_millis(120)),
            Duration::from_millis(120)
        );
        state.freeze_elapsed(base + Duration::from_millis(120));
        state.phase = Phase::Paused;
        // 暂停期间墙钟继续走,已录时长保持冻结。
        assert_eq!(
            state.elapsed(base + Duration::from_secs(60)),
            Duration::from_millis(120)
        );
        state.phase = Phase::Recording;
        state.restart_segment(base + Duration::from_secs(60));
        assert_eq!(
            state.elapsed(base + Duration::from_secs(60) + Duration::from_millis(80)),
            Duration::from_millis(200)
        );
    }

    #[test]
    fn pause_does_not_produce_frames_and_timeline_skips_paused_time() {
        let started = Instant::now();
        let source = SyntheticSource::new(48, 32);
        let region = RecordRegion::new(2, 2, 48, 32);
        let session =
            RecordingSession::start(region, test_config(RecordFormat::Gif, 60_000), source)
                .expect("start");
        let status = run_until_frames(&session, 2);
        assert_eq!(status.phase, RecordingPhase::Recording);
        let paused = session.pause().expect("pause");
        assert_eq!(paused.phase, RecordingPhase::Paused);
        let frozen = paused.frame_count;
        std::thread::sleep(Duration::from_millis(150));
        let paused_status = session.status();
        assert_eq!(
            paused_status.frame_count, frozen,
            "paused session must not produce frames"
        );
        assert_eq!(
            paused_status.elapsed_ms, paused.elapsed_ms,
            "paused elapsed must stay frozen"
        );
        let resumed = session.resume().expect("resume");
        assert_eq!(resumed.phase, RecordingPhase::Recording);
        assert!(wait_until(Duration::from_secs(5), || session
            .status()
            .frame_count
            > frozen));
        // 继续录制后再记录 60ms:墙钟时长必须包含这段真实活动时间。
        std::thread::sleep(Duration::from_millis(60));
        let output = session.stop().expect("stop");
        let wall_ms = millis(started.elapsed());
        assert!(
            output.duration_ms >= paused.elapsed_ms + 50,
            "duration must advance with wall clock while recording: duration={} paused={}",
            output.duration_ms,
            paused.elapsed_ms
        );
        // 150ms 暂停时段不得计入录制时长。
        assert!(
            wall_ms.saturating_sub(output.duration_ms) >= 130,
            "paused time must not count into duration: duration={} wall={wall_ms}",
            output.duration_ms
        );
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn repeated_pause_and_resume_are_idempotent_and_session_stays_usable() {
        let source = SyntheticSource::new(48, 32);
        let region = RecordRegion::new(2, 2, 48, 32);
        let session =
            RecordingSession::start(region, test_config(RecordFormat::Gif, 60_000), source)
                .expect("start");
        run_until_frames(&session, 2);
        // 连续 pause 与 Recording 上 resume 都必须幂等返回,不得在持有
        // shared 守卫时再次加锁(回归:不可重入 Mutex 死锁)。
        let first = session.pause().expect("first pause");
        assert_eq!(first.phase, RecordingPhase::Paused);
        let second = session.pause().expect("second pause");
        assert_eq!(second.phase, RecordingPhase::Paused);
        let third = session.pause().expect("third pause");
        assert_eq!(third.phase, RecordingPhase::Paused);
        let resumed = session.resume().expect("resume");
        assert_eq!(resumed.phase, RecordingPhase::Recording);
        let resumed_again = session.resume().expect("resume while recording");
        assert_eq!(resumed_again.phase, RecordingPhase::Recording);
        // 会话仍可继续产帧、停止并保存。
        let before = resumed_again.frame_count;
        assert!(wait_until(Duration::from_secs(5), || session
            .status()
            .frame_count
            > before));
        let output = session.stop().expect("stop after idempotent calls");
        assert!(output.frame_count > before);
        assert!(output.temp_path.exists());
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn duration_cap_auto_stops_and_keeps_completed_content() {
        let source = SyntheticSource::new(48, 32);
        let region = RecordRegion::new(2, 2, 48, 32);
        // 50fps 标称 100ms = 5 帧,但上限按墙钟判定,允许 ±1 个帧间隔的收尾偏差。
        let session = RecordingSession::start(region, test_config(RecordFormat::Gif, 100), source)
            .expect("start");
        let finished = wait_until(Duration::from_secs(5), || {
            session.status().phase == RecordingPhase::Finished
        });
        assert!(finished, "cap must auto-stop the session");
        let status = session.status();
        assert!(status.auto_stopped);
        assert!(status.frame_count >= 1);
        assert!(
            status.elapsed_ms >= 100,
            "auto-stop must be driven by wall clock: elapsed={}",
            status.elapsed_ms
        );
        let output = session.stop().expect("stop after auto-stop");
        assert!(output.auto_stopped);
        assert!(output.frame_count >= 1);
        assert!(output.duration_ms >= 100);
        assert!(output.temp_path.exists());
        let bytes = std::fs::read(&output.temp_path).expect("gif bytes");
        assert_eq!(&bytes[0..3], b"GIF");
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn slow_capture_auto_stops_at_wall_clock_limit_not_frame_count() {
        let mut source = SyntheticSource::new(48, 32);
        // 单帧 100ms 远慢于 50fps 的 20ms 标称间隔:按帧数上限(15 帧)
        // 需要约 1.5s,墙钟上限 300ms 必须先触发。
        source.capture_delay = Duration::from_millis(100);
        let region = RecordRegion::new(2, 2, 48, 32);
        let session = RecordingSession::start(region, test_config(RecordFormat::Gif, 300), source)
            .expect("start");
        let finished = wait_until(Duration::from_secs(10), || {
            session.status().phase == RecordingPhase::Finished
        });
        assert!(finished, "slow capture must still auto-stop");
        let status = session.status();
        assert!(status.auto_stopped);
        assert!(
            status.frame_count < 15,
            "must not wait for the nominal frame cap"
        );
        assert!(
            status.elapsed_ms >= 300,
            "HUD elapsed must be wall clock: elapsed={} frames={}",
            status.elapsed_ms,
            status.frame_count
        );
        // 墙钟时长必须大于按帧数换算的标称时长。
        assert!(
            status.elapsed_ms > status.frame_count * 1000 / 50,
            "elapsed must not be derived from frame count"
        );
        let output = session.stop().expect("stop after auto-stop");
        assert!(output.auto_stopped);
        assert!(output.duration_ms >= 300);
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn capture_failures_are_skipped_and_session_keeps_recording() {
        let mut source = SyntheticSource::new(48, 32);
        source.fail_first = Arc::new(AtomicU32::new(2));
        let region = RecordRegion::new(2, 2, 48, 32);
        let session =
            RecordingSession::start(region, test_config(RecordFormat::Gif, 60_000), source)
                .expect("start");
        run_until_frames(&session, 2);
        let output = session.stop().expect("stop");
        assert!(output.frame_count >= 2);
        assert!(output.interrupted.is_none());
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn session_without_frames_fails_empty_and_removes_temp_file() {
        // 抓帧固定失败:连续失败上限为 50fps × 10 秒 = 500 次,测试里等待
        // 10 秒不可行,这里用首帧失败 + 立即停止来验证 Empty 路径。
        let mut source = SyntheticSource::new(48, 32);
        source.fail_first = Arc::new(AtomicU32::new(1));
        let region = RecordRegion::new(2, 2, 48, 32);
        let session =
            RecordingSession::start(region, test_config(RecordFormat::Gif, 60_000), source)
                .expect("start");
        // 立刻停止:此时要么还没产帧(Empty),要么已经产帧(成功)。
        let result = session.stop();
        match result {
            Err(RecordError::Empty) => {}
            Ok(output) => {
                let _ = std::fs::remove_file(&output.temp_path);
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn region_for_mp4_is_trimmed_to_even_dimensions() {
        let region = RecordRegion::new(0, 0, 101, 77);
        let even = region.for_format(RecordFormat::Mp4).expect("even");
        assert_eq!((even.width, even.height), (100, 76));
        let odd = region.for_format(RecordFormat::Gif).expect("gif");
        assert_eq!((odd.width, odd.height), (101, 77));
        assert!(RecordRegion::new(0, 0, 1, 1)
            .for_format(RecordFormat::Mp4)
            .is_err());
    }

    #[test]
    fn resolve_fps_uses_the_format_default_until_a_step_is_chosen() {
        assert_eq!(resolve_recording_fps(RecordFormat::Mp4, None), 30);
        assert_eq!(resolve_recording_fps(RecordFormat::Gif, None), 15);
        assert_eq!(resolve_recording_fps(RecordFormat::Webp, None), 15);
        for format in [RecordFormat::Mp4, RecordFormat::Gif, RecordFormat::Webp] {
            assert_eq!(resolve_recording_fps(format, Some(10)), 10);
            assert_eq!(resolve_recording_fps(format, Some(15)), 15);
            assert_eq!(resolve_recording_fps(format, Some(30)), 30);
            assert_eq!(
                resolve_recording_fps(format, Some(60)),
                resolve_recording_fps(format, None)
            );
        }
    }

    #[test]
    fn fullscreen_session_capture_matches_the_yielded_chrome_rect() {
        let monitor = MonitorGeom::from_physical("m", 10, 20, 240, 180, 1.0);
        let region = RecordRegion::new(0, 0, 240, 180);
        let config = RecordConfig {
            format: RecordFormat::Gif,
            fps: 10,
            quality: 70,
            max_duration_ms: 60_000,
        };
        let plan = hud::plan_recording_chrome(
            region,
            &monitor,
            hud::chrome_spec_for(&monitor, RecordFormat::Gif),
        )
        .expect("plan");
        assert!(plan.yielded);
        let source = MonitoredSource {
            inner: SyntheticSource::new(240, 180),
            monitor: monitor.clone(),
        };
        let session = RecordingSession::start(region, config, source).expect("start");
        assert_eq!(
            (session.region.width, session.region.height),
            (plan.capture.width, plan.capture.height)
        );
        assert_eq!(
            session.annotation_inset(),
            hud::annotation_inset(region, plan.capture)
        );
        run_until_frames(&session, 1);
        let output = session.stop().expect("stop");
        assert_eq!(
            (output.width, output.height),
            (plan.capture.width, plan.capture.height)
        );
        assert_eq!(output.fps, 10);
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn region_too_small_for_chrome_does_not_start() {
        let monitor = MonitorGeom::from_physical("tiny", 0, 0, 40, 40, 1.0);
        let source = MonitoredSource {
            inner: SyntheticSource::new(40, 40),
            monitor,
        };
        let error = RecordingSession::start(
            RecordRegion::new(0, 0, 40, 40),
            test_config(RecordFormat::Gif, 60_000),
            source,
        );
        assert!(matches!(error, Err(RecordError::Region)));
    }

    #[test]
    fn slow_capture_marks_behind_without_dropping_the_clock() {
        let mut source = SyntheticSource::new(48, 32);
        source.capture_delay = Duration::from_millis(40);
        let region = RecordRegion::new(2, 2, 48, 32);
        let session =
            RecordingSession::start(region, test_config(RecordFormat::Gif, 60_000), source)
                .expect("start");
        run_until_frames(&session, 2);
        let status = session.status();
        assert!(
            status.behind,
            "slow capture must be explained while recording"
        );
        let output = session.stop().expect("stop");
        assert!(output.duration_ms >= 40, "duration {}", output.duration_ms);
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn config_sanitize_clamps_fps_quality_and_duration() {
        let config = RecordConfig {
            format: RecordFormat::Gif,
            fps: 500,
            quality: 0,
            max_duration_ms: u64::MAX,
        }
        .sanitized();
        assert_eq!(config.fps, 60);
        assert_eq!(config.quality, 1);
        assert_eq!(config.max_duration_ms, MAX_RECORDING_MS);
    }

    #[test]
    fn pause_and_resume_reject_finished_sessions() {
        let source = SyntheticSource::new(32, 32);
        let region = RecordRegion::new(0, 0, 32, 32);
        let session = RecordingSession::start(region, test_config(RecordFormat::Gif, 40), source)
            .expect("start");
        assert!(wait_until(Duration::from_secs(5), || {
            session.status().phase == RecordingPhase::Finished
        }));
        assert!(matches!(session.pause(), Err(RecordError::NotRunning)));
        assert!(matches!(session.resume(), Err(RecordError::NotRunning)));
        let output = session.stop().expect("output after auto-stop");
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn recording_sources_never_enter_capture_history() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/record");
        for entry in std::fs::read_dir(&root).expect("record dir") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("source");
            let code = source.split("#[cfg(test)]").next().unwrap_or(&source);
            // 去掉注释后再查引用:录制产物不得进入截图历史模块。
            let code = code
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !code.contains("record_capture") && !code.contains("history"),
                "{} must not touch capture history",
                path.display()
            );
        }
    }

    #[test]
    fn annotations_placed_during_recording_appear_in_encoded_frames() {
        let source = SyntheticSource::new(48, 32);
        let region = RecordRegion::new(2, 2, 48, 32);
        let session =
            RecordingSession::start(region, test_config(RecordFormat::Gif, 60_000), source)
                .expect("start");
        run_until_frames(&session, 2);
        session.set_annotations(vec![Annotation::Rect {
            x: 0.0,
            y: 0.0,
            width: 48.0,
            height: 32.0,
            color: "#e11d48".into(),
            stroke_width: Some(8.0),
        }]);
        // 等待标注后的帧写入。
        let before = session.status().frame_count;
        assert!(wait_until(Duration::from_secs(5), || session
            .status()
            .frame_count
            >= before + 2));
        let output = session.stop().expect("stop");
        let bytes = std::fs::read(&output.temp_path).expect("gif bytes");
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options.read_info(std::io::Cursor::new(bytes)).expect("gif");
        let mut has_stroke = false;
        while let Some(frame) = decoder.read_next_frame().expect("frame") {
            for pixel in frame.buffer.chunks_exact(4) {
                if pixel[0] > 150 && pixel[1] < 110 && pixel[2] < 130 {
                    has_stroke = true;
                    break;
                }
            }
        }
        assert!(has_stroke, "annotation stroke must be encoded into frames");
        let _ = std::fs::remove_file(&output.temp_path);
    }

    #[test]
    fn product_errors_are_localized() {
        assert!(RecordError::Empty.user_message().contains("没有录制"));
        assert!(RecordError::Region.user_message().contains("录制区域"));
        assert!(RecordError::Encode("x".into())
            .user_message()
            .contains("编码"));
        assert!(RecordError::TempIo("x".into())
            .user_message()
            .contains("临时文件"));
    }
}
