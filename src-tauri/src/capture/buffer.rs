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

pub fn encode_png(frame: &Frame) -> Result<Vec<u8>, CaptureError> {
    let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| CaptureError::invalid_buffer("未初始化"))?;
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|_| CaptureError::api("无法编码 PNG。"))?;
    if png.is_empty() {
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    Ok(png)
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
}
