//! R3 录制编码器:GIF、动画 WebP 与 MP4(H.264)。
//!
//! 三个编码器都只写调用方给出的临时文件,不把整段录制留在内存里:
//! - GIF 用 `gif` 逐帧写盘;
//! - 动画 WebP 逐帧用现有直接依赖 `webp` 编码成 VP8,再按 WebP RIFF 容器
//!   规范流式追加 ANMF 帧;`webp::AnimEncoder` 的前端 API 会借用并缓存全部
//!   原始帧,30 分钟上限下内存不可接受,因此这里直接写容器;
//! - MP4 用构建期随包编译的 `openh264` 编码 H.264,再经纯 Rust `mp4` muxer
//!   写出;两者都不在运行时下载任何资源。
//!
//! 编码失败返回 `RecordError`,临时文件由 `Drop` 清理(成功收尾后除外)。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::capture::buffer::Frame;

use super::{RecordConfig, RecordError, RecordRegion};

/// 录制产物格式。停止后保存按该格式给出扩展名与对话框过滤器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordFormat {
    Gif,
    Webp,
    Mp4,
}

impl RecordFormat {
    /// 规范扩展名;保存对话框按它过滤。
    pub fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Mp4 => "mp4",
        }
    }

    /// 展示名(GIF/WebP/MP4 为通用写法,不随界面语言变化)。
    pub fn label(self) -> &'static str {
        match self {
            Self::Gif => "GIF",
            Self::Webp => "WebP",
            Self::Mp4 => "MP4",
        }
    }

    /// 已知扩展名(大小写不敏感)返回格式,其余返回 None。
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "gif" => Some(Self::Gif),
            "webp" => Some(Self::Webp),
            "mp4" => Some(Self::Mp4),
            _ => None,
        }
    }
}

/// GIF 帧尺寸上限(格式本身为 u16)。
const GIF_MAX_EDGE: u32 = u16::MAX as u32;
/// WebP 画布上限(24 位宽高减一)。
const WEBP_MAX_EDGE: u32 = 16383;
/// H.264/openh264 的尺寸上限:长边 3840、短边 2160。
const MP4_MAX_EDGE: u32 = 3840;
const MP4_MAX_OTHER_EDGE: u32 = 2160;
/// NeuQuant 量化速度(1 最慢最好、30 最快);录制按实时性优先取中间偏快。
const GIF_SPEED: i32 = 10;

/// 逐帧编码接口。`finish` 成功后才保留临时文件,失败或未收尾时由 `Drop` 清理。
/// `end_ms` 是不含暂停的录制结束时刻,最后一帧的持续时间补到这个时刻。
pub(crate) trait FrameEncoder: Send {
    fn push(&mut self, frame: &Frame, timestamp_ms: u64) -> Result<(), RecordError>;
    fn finish(self: Box<Self>, end_ms: u64) -> Result<(), RecordError>;
    fn temp_path(&self) -> &Path;
}

/// 打开指定格式的编码器并创建临时文件。区域尺寸在打开前校验,
/// 打开失败不会留下文件。
pub(crate) fn open(
    config: &RecordConfig,
    region: RecordRegion,
) -> Result<Box<dyn FrameEncoder>, RecordError> {
    validate_region(config.format, region)?;
    match config.format {
        RecordFormat::Gif => Ok(Box::new(GifEncoder::new(config, region)?)),
        RecordFormat::Webp => Ok(Box::new(WebpEncoder::new(config, region)?)),
        RecordFormat::Mp4 => Ok(Box::new(Mp4Encoder::new(config, region)?)),
    }
}

/// 按格式校验区域:大于 0,且不超出对应编码器的尺寸上限。
pub(crate) fn validate_region(
    format: RecordFormat,
    region: RecordRegion,
) -> Result<(), RecordError> {
    if region.width == 0 || region.height == 0 {
        return Err(RecordError::Region);
    }
    let longest = region.width.max(region.height);
    let shortest = region.width.min(region.height);
    let fits = match format {
        RecordFormat::Gif => longest <= GIF_MAX_EDGE,
        RecordFormat::Webp => longest <= WEBP_MAX_EDGE,
        RecordFormat::Mp4 => longest <= MP4_MAX_EDGE && shortest <= MP4_MAX_OTHER_EDGE,
    };
    if fits {
        Ok(())
    } else {
        Err(RecordError::Region)
    }
}

/// 质量档位 → H.264 量化范围(QP 越小画质越高)。沿用现有质量档位语义:
/// 高/中/低三档分别对应不同 QP 区间,录制关闭跳帧时由质量模式控制画质。
pub(crate) fn qp_range_for_quality(quality: u8) -> (u8, u8) {
    match quality.clamp(1, 100) {
        80..=100 => (10, 28),
        65..=79 => (18, 34),
        _ => (26, 42),
    }
}

/// 毫秒时间戳落到 GIF 厘秒网格。总时长与墙钟的差不超过 10ms,小于一帧。
fn gif_cs(timestamp_ms: u64) -> u64 {
    timestamp_ms / 10
}

fn temp_path(format: RecordFormat) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "cropmark-recording-{}-{stamp}-{sequence}.{}",
        std::process::id(),
        format.extension()
    ))
}

fn create_file(path: &Path) -> Result<File, RecordError> {
    File::create(path).map_err(|error| RecordError::TempIo(error.to_string()))
}

fn encode_error(detail: impl Into<String>) -> RecordError {
    RecordError::Encode(detail.into())
}

fn check_frame(frame: &Frame, width: u32, height: u32) -> Result<(), RecordError> {
    let expected = width as usize * height as usize * 4;
    if frame.width != width || frame.height != height || frame.rgba.len() != expected {
        return Err(encode_error("frame size mismatch"));
    }
    Ok(())
}

/// 取出 RGB,不把整帧铺到白底上。
/// 不透明像素保持裁剪后的屏幕颜色;带透明的像素只保留该像素自己的颜色,
/// 不用白色替换整帧。
fn frame_rgb(frame: &Frame) -> Result<Vec<u8>, RecordError> {
    let mut rgb = Vec::with_capacity(frame.width as usize * frame.height as usize * 3);
    for pixel in frame.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&[pixel[0], pixel[1], pixel[2]]);
    }
    Ok(rgb)
}

// ---------------------------------------------------------------------------
// GIF
// ---------------------------------------------------------------------------

struct GifEncoder {
    encoder: Option<gif::Encoder<BufWriter<File>>>,
    path: PathBuf,
    width: u16,
    height: u16,
    /// 上一帧的 RGB 与它开始覆盖的录制时刻。持续时间要等下一帧才知道。
    pending: Option<Vec<u8>>,
    /// 已经写进文件的厘秒终点,用来把各帧对齐到同一条墙钟网格。
    written_cs: u64,
    finished: bool,
}

impl GifEncoder {
    fn new(_config: &RecordConfig, region: RecordRegion) -> Result<Self, RecordError> {
        let path = temp_path(RecordFormat::Gif);
        let file = create_file(&path)?;
        let width = region.width as u16;
        let height = region.height as u16;
        let mut encoder = gif::Encoder::new(BufWriter::new(file), width, height, &[])
            .map_err(|error| encode_error(error.to_string()))?;
        encoder
            .set_repeat(gif::Repeat::Infinite)
            .map_err(|error| encode_error(error.to_string()))?;
        Ok(Self {
            encoder: Some(encoder),
            path,
            width,
            height,
            pending: None,
            written_cs: 0,
            finished: false,
        })
    }

    fn write_rgb(&mut self, rgb: &[u8], delay_cs: u16) -> Result<(), RecordError> {
        let mut gif_frame = gif::Frame::from_rgb_speed(self.width, self.height, rgb, GIF_SPEED);
        gif_frame.delay = delay_cs.max(1);
        self.encoder
            .as_mut()
            .ok_or_else(|| encode_error("gif encoder already finished"))?
            .write_frame(&gif_frame)
            .map_err(|error| encode_error(error.to_string()))
    }
}

impl FrameEncoder for GifEncoder {
    fn push(&mut self, frame: &Frame, timestamp_ms: u64) -> Result<(), RecordError> {
        check_frame(frame, u32::from(self.width), u32::from(self.height))?;
        let rgb = frame_rgb(frame)?;
        // 第一帧从 0 开始覆盖,抓第一帧花掉的时间也算进成片。
        let timestamp_ms = if self.pending.is_none() {
            0
        } else {
            timestamp_ms
        };
        if let Some(pending) = self.pending.take() {
            let end_cs = gif_cs(timestamp_ms);
            if end_cs > self.written_cs {
                let delay = u16::try_from(end_cs - self.written_cs).unwrap_or(u16::MAX);
                self.write_rgb(&pending, delay)?;
                self.written_cs = end_cs;
                self.pending = Some(rgb);
            } else {
                // 仍落在同一厘秒:保留更新的画面,持续时间继续留给后面的帧。
                self.pending = Some(rgb);
            }
        } else {
            self.pending = Some(rgb);
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>, end_ms: u64) -> Result<(), RecordError> {
        let Some(pending) = self.pending.take() else {
            return Err(RecordError::Empty);
        };
        let end_cs = gif_cs(end_ms);
        // 厘秒网格可能让最后一档为 0;补 1 个厘秒,和墙钟的差仍不超过一帧。
        let delay =
            u16::try_from(end_cs.saturating_sub(self.written_cs).max(1)).unwrap_or(u16::MAX);
        self.write_rgb(&pending, delay)?;
        let encoder = self
            .encoder
            .take()
            .ok_or_else(|| encode_error("gif encoder already finished"))?;
        let mut writer = encoder
            .into_inner()
            .map_err(|error| encode_error(error.to_string()))?;
        writer
            .flush()
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        self.finished = true;
        Ok(())
    }

    fn temp_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for GifEncoder {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// 动画 WebP
// ---------------------------------------------------------------------------

struct WebpEncoder {
    writer: Option<AnimatedWebpWriter>,
    path: PathBuf,
    width: u32,
    height: u32,
    quality: u8,
    /// 下一帧的时长要等下一帧时间戳才知道:先压一帧,收尾时补到录制结束。
    pending: Option<(Vec<u8>, u64)>,
    finished: bool,
}

impl WebpEncoder {
    fn new(config: &RecordConfig, region: RecordRegion) -> Result<Self, RecordError> {
        let path = temp_path(RecordFormat::Webp);
        let writer = AnimatedWebpWriter::new(&path, region.width, region.height)?;
        Ok(Self {
            writer: Some(writer),
            path,
            width: region.width,
            height: region.height,
            quality: config.quality.clamp(1, 100),
            pending: None,
            finished: false,
        })
    }

    fn flush_pending(&mut self, duration_ms: u32) -> Result<(), RecordError> {
        let Some((rgb, _timestamp)) = self.pending.take() else {
            return Ok(());
        };
        self.writer
            .as_mut()
            .ok_or_else(|| encode_error("webp encoder already finished"))?
            .add_frame(&rgb, self.width, self.height, duration_ms, self.quality)
    }
}

impl FrameEncoder for WebpEncoder {
    fn push(&mut self, frame: &Frame, timestamp_ms: u64) -> Result<(), RecordError> {
        check_frame(frame, self.width, self.height)?;
        let rgb = frame_rgb(frame)?;
        let timestamp_ms = if self.pending.is_none() {
            0
        } else {
            timestamp_ms
        };
        if let Some((_, pending_timestamp)) = self.pending.as_ref() {
            let duration = timestamp_ms.saturating_sub(*pending_timestamp).max(1) as u32;
            self.flush_pending(duration)?;
        }
        self.pending = Some((rgb, timestamp_ms));
        Ok(())
    }

    fn finish(mut self: Box<Self>, end_ms: u64) -> Result<(), RecordError> {
        if let Some((_, start)) = self.pending.as_ref() {
            let duration = end_ms.saturating_sub(*start).max(1) as u32;
            self.flush_pending(duration)?;
        }
        if let Some(writer) = self.writer.take() {
            writer.finish()?;
        }
        self.finished = true;
        Ok(())
    }

    fn temp_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WebpEncoder {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 动画 WebP 容器写出:RIFF/WEBP + VP8X/ANIM 头 + 每帧 ANMF。
/// 帧数据来自 `webp::Encoder` 逐帧编码的 VP8 块。
struct AnimatedWebpWriter {
    file: BufWriter<File>,
    path: PathBuf,
}

impl AnimatedWebpWriter {
    fn new(path: &Path, width: u32, height: u32) -> Result<Self, RecordError> {
        let mut header = Vec::with_capacity(30);
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&0u32.to_le_bytes()); // 收尾时回填总长度
        header.extend_from_slice(b"WEBP");
        // VP8X:flags(animation) + 画布宽高(24 位,减一)。
        header.extend_from_slice(b"VP8X");
        header.extend_from_slice(&10u32.to_le_bytes());
        header.push(0x02);
        header.extend_from_slice(&[0, 0, 0]);
        write_u24(&mut header, width - 1);
        write_u24(&mut header, height - 1);
        // ANIM:背景色(透明黑)+ 无限循环。
        header.extend_from_slice(b"ANIM");
        header.extend_from_slice(&6u32.to_le_bytes());
        header.extend_from_slice(&[0, 0, 0, 0]);
        header.extend_from_slice(&0u16.to_le_bytes());
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        let mut file = BufWriter::new(file);
        file.write_all(&header)
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    fn add_frame(
        &mut self,
        rgb: &[u8],
        width: u32,
        height: u32,
        duration_ms: u32,
        quality: u8,
    ) -> Result<(), RecordError> {
        let memory = webp::Encoder::from_rgb(rgb, width, height).encode(f32::from(quality));
        let encoded: &[u8] = &memory;
        let payload = webp_vp8_payload(encoded)?;
        let pad = payload.len() % 2;
        let chunk_size = 16u32
            .saturating_add(8)
            .saturating_add(payload.len() as u32)
            .saturating_add(pad as u32);
        let mut header = Vec::with_capacity(24);
        header.extend_from_slice(b"ANMF");
        header.extend_from_slice(&chunk_size.to_le_bytes());
        write_u24(&mut header, 0); // 帧 X(单位 2px)
        write_u24(&mut header, 0); // 帧 Y
        write_u24(&mut header, width - 1);
        write_u24(&mut header, height - 1);
        write_u24(&mut header, duration_ms & 0x00FF_FFFF);
        header.push(0x02); // B=1(不混合)、D=0(不处置):整帧覆盖画布
        header.extend_from_slice(b"VP8 ");
        header.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.file
            .write_all(&header)
            .and_then(|_| self.file.write_all(payload))
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        if pad == 1 {
            self.file
                .write_all(&[0])
                .map_err(|error| RecordError::TempIo(error.to_string()))?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), RecordError> {
        self.file
            .flush()
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        let total = self
            .file
            .stream_position()
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        let riff_size = (total - 8) as u32;
        self.file
            .seek(SeekFrom::Start(4))
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        self.file
            .write_all(&riff_size.to_le_bytes())
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        self.file
            .flush()
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        log::info!(
            "record webp wrote bytes={total} path={}",
            self.path.display()
        );
        Ok(())
    }
}

/// 从单帧 WebP 文件里取出 VP8/VP8L 码流块(去掉 RIFF 容器)。
fn webp_vp8_payload(bytes: &[u8]) -> Result<&[u8], RecordError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(encode_error("webp container malformed"));
    }
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let fourcc = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| encode_error("webp chunk size"))?,
        ) as usize;
        let start = offset + 8;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| encode_error("webp chunk truncated"))?;
        if fourcc == b"VP8 " || fourcc == b"VP8L" {
            return Ok(&bytes[start..end]);
        }
        offset = end + (size % 2);
    }
    Err(encode_error("webp frame payload missing"))
}

fn write_u24(buffer: &mut Vec<u8>, value: u32) {
    let bytes = value.to_le_bytes();
    buffer.extend_from_slice(&bytes[0..3]);
}

// ---------------------------------------------------------------------------
// MP4(H.264)
// ---------------------------------------------------------------------------

struct PendingMp4 {
    bytes: Vec<u8>,
    is_sync: bool,
    timestamp_ms: u64,
}

struct Mp4Encoder {
    encoder: openh264::encoder::Encoder,
    writer: Option<mp4::Mp4Writer<BufWriter<File>>>,
    path: PathBuf,
    width: u16,
    height: u16,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// 上一帧样本。持续时间用下一帧的录制时刻减去这一帧的时刻。
    pending: Option<PendingMp4>,
    finished: bool,
}

impl Mp4Encoder {
    fn new(config: &RecordConfig, region: RecordRegion) -> Result<Self, RecordError> {
        if region.width % 2 != 0 || region.height % 2 != 0 {
            return Err(RecordError::Region);
        }
        let width = region.width as u16;
        let height = region.height as u16;
        let fps = config.fps.max(1);
        let (qp_min, qp_max) = qp_range_for_quality(config.quality);
        let encoder_config = openh264::encoder::EncoderConfig::new()
            .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps as f32))
            .usage_type(openh264::encoder::UsageType::ScreenContentRealTime)
            .rate_control_mode(openh264::encoder::RateControlMode::Quality)
            .qp(openh264::encoder::QpRange::new(qp_min, qp_max))
            // 录屏不能跳帧:跳帧会压缩录制时间轴。
            .skip_frames(false)
            // 屏幕内容模式下这两项本就会被编码器关闭,显式关掉避免告警噪声。
            .adaptive_quantization(false)
            .background_detection(false)
            .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(
                fps.saturating_mul(5),
            ))
            // openh264 的 RGBA→I420 转换使用 BT.601 系数(limited range),
            // VUI 按实际转换矩阵标注,避免播放器按 BT.709 解读造成偏色。
            .vui(openh264::encoder::VuiConfig::bt601());
        let encoder = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            encoder_config,
        )
        .map_err(|error| encode_error(error.to_string()))?;
        Ok(Self {
            encoder,
            writer: None,
            path: temp_path(RecordFormat::Mp4),
            width,
            height,
            sps: Vec::new(),
            pps: Vec::new(),
            pending: None,
            finished: false,
        })
    }

    fn start_writer(&mut self) -> Result<(), RecordError> {
        if self.sps.is_empty() || self.pps.is_empty() {
            return Err(encode_error("h264 parameter sets missing"));
        }
        let file = match std::fs::File::create(&self.path) {
            Ok(file) => file,
            Err(error) => return Err(RecordError::TempIo(error.to_string())),
        };
        let avc = mp4::AvcConfig {
            width: self.width,
            height: self.height,
            seq_param_set: self.sps.clone(),
            pic_param_set: self.pps.clone(),
        };
        let mp4_config = mp4::Mp4Config {
            major_brand: str::parse("isom").map_err(|_| encode_error("mp4 brand"))?,
            minor_version: 512,
            compatible_brands: ["isom", "iso2", "avc1", "mp41"]
                .iter()
                .map(|brand| str::parse(brand).map_err(|_| encode_error("mp4 brand")))
                .collect::<Result<Vec<_>, _>>()?,
            // 时间刻度用毫秒,样本时长才能写成真实录制间隔,而不是恒定的 1/fps。
            timescale: 1000,
        };
        let mut writer = mp4::Mp4Writer::write_start(BufWriter::new(file), &mp4_config)
            .map_err(|error| encode_error(error.to_string()))?;
        writer
            .add_track(&mp4::TrackConfig {
                track_type: mp4::TrackType::Video,
                timescale: 1000,
                language: String::from("und"),
                media_conf: mp4::MediaConfig::AvcConfig(avc),
            })
            .map_err(|error| encode_error(error.to_string()))?;
        self.writer = Some(writer);
        Ok(())
    }

    fn write_pending(&mut self, next_timestamp_ms: u64) -> Result<(), RecordError> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let duration = u32::try_from(
            next_timestamp_ms
                .saturating_sub(pending.timestamp_ms)
                .max(1),
        )
        .unwrap_or(u32::MAX);
        let sample = mp4::Mp4Sample {
            start_time: pending.timestamp_ms,
            duration,
            rendering_offset: 0,
            is_sync: pending.is_sync,
            bytes: pending.bytes.into(),
        };
        self.writer
            .as_mut()
            .ok_or_else(|| encode_error("mp4 writer missing"))?
            .write_sample(1, &sample)
            .map_err(|error| encode_error(error.to_string()))
    }
}

impl FrameEncoder for Mp4Encoder {
    fn push(&mut self, frame: &Frame, timestamp_ms: u64) -> Result<(), RecordError> {
        check_frame(frame, u32::from(self.width), u32::from(self.height))?;
        let yuv =
            openh264::formats::YUVBuffer::from_rgba8_source(openh264::formats::RgbaSliceU8::new(
                &frame.rgba,
                (usize::from(self.width), usize::from(self.height)),
            ));
        let (nals, frame_type) = {
            let bitstream = self
                .encoder
                .encode(&yuv)
                .map_err(|error| encode_error(error.to_string()))?;
            let mut nals: Vec<Vec<u8>> = Vec::new();
            for index in 0..bitstream.num_layers() {
                let Some(layer) = bitstream.layer(index) else {
                    continue;
                };
                // SPS/PPS 位于非视频层,不能按 `is_video` 过滤。
                for nal_index in 0..layer.nal_count() {
                    let Some(nal) = layer.nal_unit(nal_index) else {
                        continue;
                    };
                    // OpenH264 输出为 Annex B(带起始码),avcC 与样本需要裸 NAL。
                    let payload = strip_start_code(nal);
                    let nal_type = payload.first().map_or(0, |byte| byte & 0x1F);
                    if nal_type == 7 {
                        if self.sps.is_empty() {
                            self.sps = payload.to_vec();
                        }
                    } else if nal_type == 8 {
                        if self.pps.is_empty() {
                            self.pps = payload.to_vec();
                        }
                    } else {
                        nals.push(payload.to_vec());
                    }
                }
            }
            (nals, bitstream.frame_type())
        };
        if self.writer.is_none() {
            self.start_writer()?;
        }
        let timestamp_ms = if self.pending.is_none() {
            0
        } else {
            timestamp_ms
        };
        self.write_pending(timestamp_ms)?;
        let is_sync = matches!(
            frame_type,
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        self.pending = Some(PendingMp4 {
            bytes: avcc_sample(&nals),
            is_sync,
            timestamp_ms,
        });
        Ok(())
    }

    fn finish(mut self: Box<Self>, end_ms: u64) -> Result<(), RecordError> {
        if self.pending.is_none() {
            return Err(RecordError::Empty);
        }
        self.write_pending(end_ms.max(1))?;
        let Some(mut writer) = self.writer.take() else {
            return Err(RecordError::Empty);
        };
        writer
            .write_end()
            .map_err(|error| encode_error(error.to_string()))?;
        writer
            .into_writer()
            .flush()
            .map_err(|error| RecordError::TempIo(error.to_string()))?;
        self.finished = true;
        log::info!(
            "record mp4 wrote size={}x{} path={}",
            self.width,
            self.height,
            self.path.display()
        );
        Ok(())
    }

    fn temp_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Mp4Encoder {
    fn drop(&mut self) {
        if !self.finished {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 去掉 Annex B 起始码(3 或 4 字节);没有起始码时原样返回。
fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.len() >= 4 && nal[0] == 0 && nal[1] == 0 && nal[2] == 0 && nal[3] == 1 {
        return &nal[4..];
    }
    if nal.len() >= 3 && nal[0] == 0 && nal[1] == 0 && nal[2] == 1 {
        return &nal[3..];
    }
    nal
}

/// 每个 NAL 前置 4 字节长度成 AVCC 样本。
fn avcc_sample(nals: &[Vec<u8>]) -> Vec<u8> {
    let total: usize = nals.iter().map(|nal| 4 + nal.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, RawBuffer};
    use openh264::formats::YUVSource;

    fn test_frame(width: u32, height: u32, shift: u32) -> Frame {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let bright = ((x + y * 2 + shift) % 32) < 16;
                let value = if bright { 235 } else { 40 };
                bytes.extend_from_slice(&[value, value, 200, 255]);
            }
        }
        accept_buffer(RawBuffer::ready(width, height, bytes)).expect("frame")
    }

    fn decode_gif(bytes: &[u8]) -> Vec<(u16, u16, u16)> {
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options.read_info(std::io::Cursor::new(bytes)).expect("gif");
        let mut frames = Vec::new();
        while let Some(frame) = decoder.read_next_frame().expect("gif frame") {
            frames.push((frame.width, frame.height, frame.delay));
        }
        frames
    }

    #[test]
    fn gif_encoder_pushes_every_frame_with_fixed_delay() {
        let config = RecordConfig {
            format: RecordFormat::Gif,
            fps: 10,
            quality: 80,
            max_duration_ms: 1_000,
        };
        let region = RecordRegion {
            x: 0,
            y: 0,
            width: 32,
            height: 24,
        };
        let mut encoder = open(&config, region).expect("gif encoder");
        let path = encoder.temp_path().to_path_buf();
        for index in 0..3 {
            encoder
                .push(&test_frame(32, 24, index * 3), u64::from(index) * 100)
                .expect("push");
        }
        encoder.finish(300).expect("finish");
        let bytes = std::fs::read(&path).expect("gif bytes");
        let frames = decode_gif(&bytes);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0], (32, 24, 10));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn webp_encoder_writes_decodable_animation_with_durations() {
        let config = RecordConfig {
            format: RecordFormat::Webp,
            fps: 10,
            quality: 80,
            max_duration_ms: 1_000,
        };
        let region = RecordRegion {
            x: 0,
            y: 0,
            width: 32,
            height: 24,
        };
        let mut encoder = open(&config, region).expect("webp encoder");
        let path = encoder.temp_path().to_path_buf();
        for index in 0..3 {
            encoder
                .push(&test_frame(32, 24, index * 5), u64::from(index) * 100)
                .expect("push");
        }
        encoder.finish(300).expect("finish");
        let bytes = std::fs::read(&path).expect("webp bytes");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WEBP");
        let decoded = webp::AnimDecoder::new(&bytes)
            .decode()
            .expect("anim decode");
        assert!(decoded.has_animation());
        assert_eq!(decoded.len(), 3);
        let first = decoded.get_frame(0).expect("first frame");
        assert_eq!(first.width(), 32);
        assert_eq!(first.height(), 24);
        // 解码时间戳为累计结束时间:每帧 100ms → 100/200/300。
        let times: Vec<i32> = (0..decoded.len())
            .map(|index| decoded.get_frame(index).expect("frame").get_time_ms())
            .collect();
        assert_eq!(times, vec![100, 200, 300]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mp4_encoder_writes_h264_stream_decodable_by_openh264() {
        let config = RecordConfig {
            format: RecordFormat::Mp4,
            fps: 10,
            quality: 80,
            max_duration_ms: 1_000,
        };
        let region = RecordRegion {
            x: 0,
            y: 0,
            width: 64,
            height: 48,
        };
        let mut encoder = open(&config, region).expect("mp4 encoder");
        let path = encoder.temp_path().to_path_buf();
        for index in 0..3 {
            encoder
                .push(&test_frame(64, 48, index * 7), u64::from(index) * 100)
                .expect("push");
        }
        encoder.finish(300).expect("finish");

        let bytes = std::fs::read(&path).expect("mp4 bytes");
        let mut reader =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(bytes.clone()), bytes.len() as u64)
                .expect("mp4 header");
        let (track_id, sample_count, width, height, sps, pps) = {
            let (id, track) = reader.tracks().iter().next().expect("video track");
            (
                *id,
                track.sample_count(),
                track.width(),
                track.height(),
                track.sequence_parameter_set().expect("sps").to_vec(),
                track.picture_parameter_set().expect("pps").to_vec(),
            )
        };
        assert_eq!(sample_count, 3);
        assert_eq!((width, height), (64, 48));
        let sample = reader
            .read_sample(track_id, 1)
            .expect("read sample")
            .expect("sample present");
        assert!(sample.is_sync);
        let mut annexb = Vec::new();
        for nal in annex_b_units(&sample.bytes) {
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(nal);
        }
        let mut full = Vec::new();
        for parameter in [&sps, &pps] {
            full.extend_from_slice(&[0, 0, 0, 1]);
            full.extend_from_slice(parameter);
        }
        full.extend_from_slice(&annexb);
        let mut decoder = openh264::decoder::Decoder::new().expect("h264 decoder");
        let decoded = decoder
            .decode(&full)
            .expect("h264 decode")
            .expect("decoded picture");
        assert_eq!(decoded.dimensions(), (64, 48));
        let _ = std::fs::remove_file(&path);
    }

    /// AVCC(4 字节长度前缀)样本 → NAL 列表。
    fn annex_b_units(sample: &[u8]) -> Vec<&[u8]> {
        let mut units = Vec::new();
        let mut offset = 0usize;
        while offset + 4 <= sample.len() {
            let size = u32::from_be_bytes(sample[offset..offset + 4].try_into().unwrap()) as usize;
            let start = offset + 4;
            let end = start + size;
            if end > sample.len() {
                break;
            }
            units.push(&sample[start..end]);
            offset = end;
        }
        units
    }

    #[test]
    fn mp4_roundtrip_keeps_the_dominant_color() {
        let config = RecordConfig {
            format: RecordFormat::Mp4,
            fps: 10,
            quality: 90,
            max_duration_ms: 1_000,
        };
        let region = RecordRegion {
            x: 0,
            y: 0,
            width: 64,
            height: 48,
        };
        let solid = accept_buffer(RawBuffer::ready(
            64,
            48,
            [200u8, 30, 40, 255].repeat(64 * 48),
        ))
        .expect("solid frame");
        let mut encoder = open(&config, region).expect("mp4 encoder");
        let path = encoder.temp_path().to_path_buf();
        encoder.push(&solid, 0).expect("push");
        encoder.finish(100).expect("finish");

        let bytes = std::fs::read(&path).expect("mp4 bytes");
        let mut reader =
            mp4::Mp4Reader::read_header(std::io::Cursor::new(bytes.clone()), bytes.len() as u64)
                .expect("mp4 header");
        let (track_id, sps, pps) = {
            let (id, track) = reader.tracks().iter().next().expect("track");
            (
                *id,
                track.sequence_parameter_set().expect("sps").to_vec(),
                track.picture_parameter_set().expect("pps").to_vec(),
            )
        };
        let sample = reader.read_sample(track_id, 1).unwrap().expect("sample");
        let mut annexb = Vec::new();
        for parameter in [&sps, &pps] {
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(parameter);
        }
        for nal in annex_b_units(&sample.bytes) {
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(nal);
        }
        let mut decoder = openh264::decoder::Decoder::new().expect("decoder");
        let decoded = decoder.decode(&annexb).unwrap().expect("picture");
        let (width, height) = decoded.dimensions();
        let mut rgb = vec![0u8; width * height * 3];
        decoded.write_rgb8(&mut rgb);
        let center = ((height / 2) * width + width / 2) * 3;
        let (red, green, blue) = (rgb[center], rgb[center + 1], rgb[center + 2]);
        assert!(
            red > green + 50 && red > blue + 50,
            "decoded color must stay red-dominant: ({red},{green},{blue})"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn quality_tiers_map_to_distinct_qp_ranges() {
        let (high_min, high_max) = qp_range_for_quality(90);
        let (medium_min, medium_max) = qp_range_for_quality(75);
        let (low_min, low_max) = qp_range_for_quality(55);
        assert!(high_min < medium_min && medium_min < low_min);
        assert!(high_max < medium_max && medium_max < low_max);
        // 越界输入钳制后仍有合法区间。
        let (min, max) = qp_range_for_quality(0);
        assert!(min <= max && max <= 51);
    }

    #[test]
    fn annex_b_start_codes_are_stripped_for_avcc_samples() {
        assert_eq!(strip_start_code(&[0, 0, 0, 1, 0x65, 0x88]), &[0x65, 0x88]);
        assert_eq!(strip_start_code(&[0, 0, 1, 0x67, 0x42]), &[0x67, 0x42]);
        assert_eq!(strip_start_code(&[0x65, 0x88]), &[0x65, 0x88]);
        let sample = avcc_sample(&[vec![1, 2, 3], vec![4, 5]]);
        assert_eq!(sample, vec![0, 0, 0, 3, 1, 2, 3, 0, 0, 0, 2, 4, 5]);
    }

    #[test]
    fn region_limits_are_checked_before_creating_files() {
        let small = RecordRegion {
            x: 0,
            y: 0,
            width: 64,
            height: 48,
        };
        assert!(validate_region(RecordFormat::Mp4, small).is_ok());
        assert!(validate_region(
            RecordFormat::Mp4,
            RecordRegion {
                width: 64,
                height: 0,
                ..small
            }
        )
        .is_err());
        assert!(validate_region(
            RecordFormat::Gif,
            RecordRegion {
                width: 70_000,
                height: 48,
                ..small
            }
        )
        .is_err());
        let config = RecordConfig {
            format: RecordFormat::Mp4,
            fps: 10,
            quality: 80,
            max_duration_ms: 1_000,
        };
        assert!(open(
            &config,
            RecordRegion {
                width: 63,
                height: 48,
                ..small
            }
        )
        .is_err());
    }

    #[test]
    fn dropped_encoder_removes_unfinished_temp_file() {
        let config = RecordConfig {
            format: RecordFormat::Gif,
            fps: 10,
            quality: 80,
            max_duration_ms: 1_000,
        };
        let region = RecordRegion {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        };
        let encoder = open(&config, region).expect("encoder");
        let path = encoder.temp_path().to_path_buf();
        assert!(path.exists());
        drop(encoder);
        assert!(!path.exists(), "unfinished temp file must be removed");
    }

    fn solid(width: u32, height: u32, pixel: [u8; 4]) -> Frame {
        accept_buffer(RawBuffer::ready(
            width,
            height,
            pixel.repeat((width * height) as usize),
        ))
        .expect("solid")
    }

    #[test]
    fn slow_frames_stretch_to_the_recording_clock() {
        // 第二帧晚了 300ms:上一帧拉长补上,成片总时长仍是结束时刻。
        for format in [RecordFormat::Gif, RecordFormat::Webp, RecordFormat::Mp4] {
            let wide = if format == RecordFormat::Mp4 { 64 } else { 32 };
            let tall = if format == RecordFormat::Mp4 { 48 } else { 24 };
            let config = RecordConfig {
                format,
                fps: 10,
                quality: 80,
                max_duration_ms: 1_000,
            };
            let region = RecordRegion {
                x: 0,
                y: 0,
                width: wide,
                height: tall,
            };
            let mut encoder = open(&config, region).expect("encoder");
            let path = encoder.temp_path().to_path_buf();
            for (index, timestamp) in [0u64, 100, 400].into_iter().enumerate() {
                encoder
                    .push(&test_frame(wide, tall, index as u32 * 3), timestamp)
                    .expect("push");
            }
            encoder.finish(450).expect("finish");
            match format {
                RecordFormat::Gif => {
                    let frames = decode_gif(&std::fs::read(&path).expect("gif"));
                    let delays: Vec<u16> = frames.iter().map(|frame| frame.2).collect();
                    assert_eq!(delays, vec![10, 30, 5]);
                    let total_ms = delays
                        .iter()
                        .map(|delay| u64::from(*delay) * 10)
                        .sum::<u64>();
                    assert!(
                        total_ms.abs_diff(450) <= 100,
                        "gif clock drifted past one frame: {total_ms}"
                    );
                }
                RecordFormat::Webp => {
                    let bytes = std::fs::read(&path).expect("webp");
                    let decoded = webp::AnimDecoder::new(&bytes).decode().expect("webp");
                    let times: Vec<i32> = (0..decoded.len())
                        .map(|index| decoded.get_frame(index).expect("frame").get_time_ms())
                        .collect();
                    assert_eq!(times, vec![100, 400, 450]);
                }
                RecordFormat::Mp4 => {
                    let bytes = std::fs::read(&path).expect("mp4");
                    let mut reader = mp4::Mp4Reader::read_header(
                        std::io::Cursor::new(bytes.clone()),
                        bytes.len() as u64,
                    )
                    .expect("mp4");
                    let track_id = {
                        let (id, _) = reader.tracks().iter().next().expect("track");
                        *id
                    };
                    let mut total = 0u64;
                    for index in 1..=3 {
                        let sample = reader
                            .read_sample(track_id, index)
                            .expect("read")
                            .expect("sample");
                        total += sample.duration as u64;
                    }
                    assert_eq!(total, 450);
                }
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn gif_does_not_matte_dark_or_transparent_pixels_onto_white() {
        let config = RecordConfig {
            format: RecordFormat::Gif,
            fps: 15,
            quality: 80,
            max_duration_ms: 1_000,
        };
        let region = RecordRegion {
            x: 0,
            y: 0,
            width: 32,
            height: 24,
        };
        let assert_dark = |pixel: [u8; 4]| {
            let mut encoder = open(&config, region).expect("gif");
            let path = encoder.temp_path().to_path_buf();
            encoder.push(&solid(32, 24, pixel), 0).expect("push");
            encoder.finish(100).expect("finish");
            let bytes = std::fs::read(&path).expect("gif");
            let mut options = gif::DecodeOptions::new();
            options.set_color_output(gif::ColorOutput::RGBA);
            let mut decoder = options.read_info(std::io::Cursor::new(bytes)).expect("gif");
            let frame = decoder.read_next_frame().expect("frame").expect("frame");
            for channel in frame.buffer.chunks_exact(4) {
                assert!(
                    channel[0] < 80 && channel[1] < 80 && channel[2] < 80,
                    "dark frame was matted toward white: {channel:?}"
                );
            }
            let _ = std::fs::remove_file(&path);
        };
        assert_dark([12, 18, 24, 255]);
        // alpha 为 0 时只动这个像素,不能把整帧铺白。
        assert_dark([0, 0, 0, 0]);
    }
}
