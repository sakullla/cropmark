use super::error::CaptureError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameInit {
    Ready,
    Uninitialized,
}

#[derive(Debug, Clone)]
pub struct RawBuffer {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
    pub init: FrameInit,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub scale: f64,
}

impl RawBuffer {
    pub fn ready(width: u32, height: u32, bytes: Vec<u8>) -> Self {
        Self {
            width,
            height,
            bytes,
            init: FrameInit::Ready,
        }
    }

    pub fn uninitialized() -> Self {
        Self {
            width: 0,
            height: 0,
            bytes: Vec::new(),
            init: FrameInit::Uninitialized,
        }
    }

    pub fn is_all_black(&self) -> bool {
        if self.bytes.len() < 4 {
            return false;
        }
        self.bytes.chunks_exact(4).all(|px| px[0] == 0 && px[1] == 0 && px[2] == 0)
    }
}

pub fn accept_buffer(raw: RawBuffer) -> Result<Frame, CaptureError> {
    if raw.init == FrameInit::Uninitialized {
        return Err(CaptureError::invalid_buffer("未初始化"));
    }
    if raw.width == 0 || raw.height == 0 {
        return Err(CaptureError::invalid_buffer("尺寸为 0"));
    }
    if raw.bytes.is_empty() {
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    let expected = raw.width as usize * raw.height as usize * 4;
    if raw.bytes.len() != expected {
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    Ok(Frame {
        width: raw.width,
        height: raw.height,
        rgba: raw.bytes,
        scale: 1.0,
    })
}

pub fn crop_rgba(frame: &Frame, x: u32, y: u32, width: u32, height: u32) -> Result<Frame, CaptureError> {
    if width == 0 || height == 0 {
        return Err(CaptureError::invalid_buffer("尺寸为 0"));
    }
    if x.saturating_add(width) > frame.width || y.saturating_add(height) > frame.height {
        return Err(CaptureError::api("选区超出截取画面。"));
    }
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for row in 0..height as usize {
        let src = ((y as usize + row) * frame.width as usize + x as usize) * 4;
        let dst = row * width as usize * 4;
        rgba[dst..dst + width as usize * 4]
            .copy_from_slice(&frame.rgba[src..src + width as usize * 4]);
    }
    Ok(Frame {
        width,
        height,
        rgba,
        scale: frame.scale,
    })
}

pub fn crop_desktop_to_monitor(
    frame: Frame,
    monitor: &super::geometry::MonitorGeom,
    desktop_origin_x: i32,
    desktop_origin_y: i32,
) -> Result<Frame, CaptureError> {
    if frame.width == monitor.physical_width && frame.height == monitor.physical_height {
        let mut frame = frame;
        frame.scale = monitor.scale;
        return Ok(frame);
    }
    let x = monitor.physical_x - desktop_origin_x;
    let y = monitor.physical_y - desktop_origin_y;
    if x < 0 || y < 0 {
        return Err(CaptureError::api("无法按指针所在屏裁剪截屏。"));
    }
    let mut cropped = crop_rgba(
        &frame,
        x as u32,
        y as u32,
        monitor.physical_width,
        monitor.physical_height,
    )?;
    cropped.scale = monitor.scale;
    Ok(cropped)
}

pub fn encode_png(frame: &Frame) -> Result<Vec<u8>, CaptureError> {
    use image::ImageEncoder;
    validate_frame(frame)?;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&frame.rgba, frame.width, frame.height, image::ExtendedColorType::Rgba8)
        .map_err(|_| CaptureError::api("无法编码 PNG。"))?;
    Ok(bytes)
}

pub fn validate_frame(frame: &Frame) -> Result<(), CaptureError> {
    let expected = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if frame.width == 0 || frame.height == 0 || expected != Some(frame.rgba.len()) {
        return Err(CaptureError::invalid_buffer("尺寸或像素长度不匹配"));
    }
    Ok(())
}

pub fn encode_jpeg(frame: &Frame, quality: u8) -> Result<Vec<u8>, CaptureError> {
    let rgba = rgba_image(frame)?;
    let mut rgb = Vec::with_capacity((rgba.width() * rgba.height() * 3) as usize);
    for pixel in rgba.pixels() {
        rgb.extend_from_slice(&[pixel[0], pixel[1], pixel[2]]);
    }
    let mut jpeg = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality);
    encoder
        .encode(
            &rgb,
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|_| CaptureError::api("无法编码预览图。"))?;
    if jpeg.is_empty() {
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    Ok(jpeg)
}

pub fn fit_display(width: u32, height: u32, max_edge: u32) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let long = width.max(height);
    if long <= max_edge {
        return (width, height);
    }
    let scale = max_edge as f64 / long as f64;
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

pub fn resize_rgba(frame: &Frame, width: u32, height: u32) -> Result<Frame, CaptureError> {
    if width == 0 || height == 0 {
        return Err(CaptureError::invalid_buffer("尺寸为 0"));
    }
    if frame.width == width && frame.height == height {
        return Ok(frame.clone());
    }
    let resized = image::imageops::resize(
        &rgba_image(frame)?,
        width,
        height,
        image::imageops::FilterType::Triangle,
    );
    Ok(Frame {
        width,
        height,
        rgba: resized.into_raw(),
        scale: frame.scale,
    })
}

fn rgba_image(frame: &Frame) -> Result<image::RgbaImage, CaptureError> {
    image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| CaptureError::invalid_buffer("未初始化"))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn decode_png(bytes: &[u8]) -> Result<Frame, CaptureError> {
    if bytes.is_empty() {
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    let image = image::load_from_memory(bytes)
        .map_err(|_| CaptureError::api("无法解码截屏图像。"))?
        .to_rgba8();
    let width = image.width();
    let height = image.height();
    accept_buffer(RawBuffer::ready(width, height, image.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_roundtrip_keeps_native_dimensions_and_every_rgba_pixel() {
        let mut frame = accept_buffer(solid(37, 19, [4, 50, 240, 255])).unwrap();
        frame.scale = 1.5;
        frame.rgba[0..4].copy_from_slice(&[255, 10, 20, 128]);
        let decoded = decode_png(&encode_png(&frame).unwrap()).unwrap();
        assert_eq!((decoded.width, decoded.height), (37, 19));
        assert_eq!(decoded.rgba, frame.rgba);
    }

    #[test]
    fn png_rejects_invalid_pixel_lengths_without_panicking() {
        let frame = Frame { width: 2, height: 2, rgba: vec![0; 4], scale: 1.0 };
        assert!(encode_png(&frame).is_err());
    }

    #[test]
    #[ignore = "manual capture encoding benchmark; run with --ignored --nocapture"]
    fn benchmark_capture_encoding() {
        use std::time::Instant;
        for (width, height) in [(1920, 1080), (3840, 2160)] {
            let mut frame = accept_buffer(solid(width, height, [245, 245, 245, 255])).unwrap();
            for (i, pixel) in frame.rgba.chunks_exact_mut(4).enumerate() {
                let x = i as u32 % width;
                let y = i as u32 / width;
                pixel[0] = (x / 8 + y / 16) as u8;
                pixel[1] = (x / 16 + y / 8) as u8;
                pixel[2] = if y % 24 < 3 { 30 } else { 235 };
            }
            let started = Instant::now();
            let png = encode_png(&frame).unwrap();
            eprintln!("{width}x{height} current PNG: {:?}, {} bytes", started.elapsed(), png.len());
            assert_eq!(decode_png(&png).unwrap().rgba, frame.rgba);
        }
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> RawBuffer {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            bytes.extend_from_slice(&rgba);
        }
        RawBuffer::ready(width, height, bytes)
    }

    #[test]
    fn all_black_valid_buffer_is_success() {
        let raw = solid(8, 8, [0, 0, 0, 255]);
        assert!(raw.is_all_black());
        let frame = accept_buffer(raw).expect("authorized black frame is success");
        assert_eq!(frame.width, 8);
        assert_eq!(frame.height, 8);
        assert!(encode_png(&frame).unwrap().starts_with(&[137, 80, 78, 71]));
    }

    #[test]
    fn empty_buffer_is_failure() {
        let error = accept_buffer(RawBuffer::ready(10, 10, Vec::new())).unwrap_err();
        assert_eq!(error.kind, super::super::error::CaptureErrorKind::InvalidBuffer);
        assert!(error.message.contains("空"));
    }

    #[test]
    fn zero_size_is_failure_even_with_bytes() {
        let error = accept_buffer(RawBuffer::ready(0, 12, vec![0; 4])).unwrap_err();
        assert!(error.message.contains("0"));
    }

    #[test]
    fn uninitialized_is_failure() {
        let error = accept_buffer(RawBuffer::uninitialized()).unwrap_err();
        assert!(error.message.contains("未初始化"));
    }

    #[test]
    fn length_mismatch_is_invalid_buffer() {
        let error = accept_buffer(RawBuffer::ready(2, 2, vec![1, 2, 3])).unwrap_err();
        assert_eq!(error.kind, super::super::error::CaptureErrorKind::InvalidBuffer);
    }

    #[test]
    fn crop_copies_physical_pixels() {
        let mut bytes = vec![0u8; 4 * 4 * 4];
        bytes[20..24].copy_from_slice(&[1, 2, 3, 4]);
        let frame = accept_buffer(RawBuffer::ready(4, 4, bytes)).unwrap();
        let cropped = crop_rgba(&frame, 1, 1, 1, 1).unwrap();
        assert_eq!(cropped.rgba, vec![1, 2, 3, 4]);
    }

    #[test]
    fn fit_display_caps_long_edge() {
        assert_eq!(fit_display(3840, 2160, 1280), (1280, 720));
        assert_eq!(fit_display(800, 600, 1280), (800, 600));
    }

    #[test]
    fn resize_rgba_keeps_requested_display_size() {
        let frame = accept_buffer(solid(8, 4, [10, 20, 30, 255])).unwrap();
        let resized = resize_rgba(&frame, 4, 2).unwrap();
        assert_eq!(resized.width, 4);
        assert_eq!(resized.height, 2);
        assert_eq!(resized.rgba.len(), 4 * 2 * 4);
    }

    #[test]
    fn portal_desktop_crops_to_pointer_monitor() {
        let mut bytes = vec![0u8; 4 * 2 * 4];
        bytes[8..12].copy_from_slice(&[9, 8, 7, 6]);
        let frame = accept_buffer(RawBuffer::ready(4, 2, bytes)).unwrap();
        let monitor = super::super::geometry::MonitorGeom::from_physical("right", 2, 0, 2, 2, 1.0);
        let cropped = crop_desktop_to_monitor(frame, &monitor, 0, 0).unwrap();
        assert_eq!(cropped.width, 2);
        assert_eq!(cropped.height, 2);
        assert_eq!(&cropped.rgba[0..4], &[9, 8, 7, 6]);
    }
}
