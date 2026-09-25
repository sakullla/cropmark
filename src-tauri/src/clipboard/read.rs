//! R8 剪贴板读取(ADR-9):图片优先,其次纯文本;空/不支持格式由调用方提示。
//!
//! - arboard 直接给出 RGBA 图片数据(Windows 上读注册的 PNG 格式与 CF_DIBV5);
//! - Windows 额外回退传统 CF_DIB:部分旧应用只放这一种 DIB,arboard 不读取;
//! - Linux Wayland 会话读取失败时给出明确文案(data-control 不可用需退回 X11)。

use arboard::Clipboard;

use crate::capture::buffer::{accept_buffer, Frame, RawBuffer};
use crate::i18n;

#[derive(Debug, Clone)]
pub enum ClipboardContent {
    Image(Frame),
    Text(String),
    /// 剪贴板有内容,但既不是可解码图片也不是非空文本。
    Unsupported,
    /// 剪贴板为空(或只有空白文本)。
    Empty,
}

/// `Frame` 不实现 `PartialEq`(避免改动采集缓冲类型);判定/测试按像素比较。
impl PartialEq for ClipboardContent {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Image(left), Self::Image(right)) => {
                left.width == right.width
                    && left.height == right.height
                    && left.rgba == right.rgba
                    && left.scale == right.scale
            }
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Empty, Self::Empty) | (Self::Unsupported, Self::Unsupported) => true,
            _ => false,
        }
    }
}

/// 纯判定(可单测):图片优先于文本;空白文本按空处理,与「无文本」区分。
pub fn classify(image: Option<Frame>, text: Option<String>) -> ClipboardContent {
    if let Some(frame) = image.filter(is_valid_frame) {
        return ClipboardContent::Image(frame);
    }
    match text {
        Some(text) if !text.trim().is_empty() => ClipboardContent::Text(text),
        Some(_) => ClipboardContent::Empty,
        None => ClipboardContent::Unsupported,
    }
}

fn is_valid_frame(frame: &Frame) -> bool {
    frame.width > 0 && frame.height > 0 && !frame.rgba.is_empty()
}

/// 读取当前剪贴板内容;宿主不可用(无显示会话/被占用等)返回可展示文案。
pub fn read_content() -> Result<ClipboardContent, String> {
    let mut clipboard = Clipboard::new().map_err(|_| unavailable_message())?;
    let image = clipboard.get_image().ok().and_then(image_to_frame);
    let text = clipboard.get_text().ok();
    // 先释放 arboard 的剪贴板句柄,Windows 兜底再以同一把锁读取 CF_DIB。
    drop(clipboard);
    let image = image.or_else(platform_fallback_image);
    Ok(classify(image, text))
}

fn image_to_frame(image: arboard::ImageData<'_>) -> Option<Frame> {
    let width = u32::try_from(image.width).ok()?;
    let height = u32::try_from(image.height).ok()?;
    accept_buffer(RawBuffer::ready(width, height, image.bytes.into_owned())).ok()
}

fn unavailable_message() -> String {
    #[cfg(target_os = "linux")]
    {
        if wayland_session() {
            return i18n::t("clipboard.hint.wayland");
        }
    }
    i18n::t("clipboard.hint.unavailable")
}

#[cfg(target_os = "linux")]
fn wayland_session() -> bool {
    std::env::var("WAYLAND_DISPLAY")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
        || std::env::var("XDG_SESSION_TYPE")
            .map(|value| value.trim().eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

/// Windows 兜底:arboard 只读 PNG 注册格式与 CF_DIBV5,传统应用常常只提供
/// CF_DIB(40 字节 BITMAPINFOHEADER);此处按同一剪贴板锁读取并自行解码。
#[cfg(windows)]
fn platform_fallback_image() -> Option<Frame> {
    use clipboard_win::formats;
    let _clipboard = clipboard_win::Clipboard::new().ok()?;
    if !clipboard_win::raw::is_format_avail(formats::CF_DIB) {
        return None;
    }
    let mut data = Vec::new();
    clipboard_win::raw::get_vec(formats::CF_DIB, &mut data).ok()?;
    parse_dib_rgba(&data)
}

#[cfg(not(windows))]
fn platform_fallback_image() -> Option<Frame> {
    None
}

/// 把 CF_DIB 缓冲区(header + 像素)解码为 RGBA。支持 24/32bpp、
/// BI_RGB 与 BI_BITFIELDS、正/负高度(下/上对齐);其余形态返回 None。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn parse_dib_rgba(bytes: &[u8]) -> Option<Frame> {
    if bytes.len() < 40 {
        return None;
    }
    let header_size = read_u32(bytes, 0)? as usize;
    if header_size < 40 || header_size > bytes.len() {
        return None;
    }
    let width = read_i32(bytes, 4)?;
    let height = read_i32(bytes, 8)?;
    let planes = read_u16(bytes, 12)?;
    let bit_count = read_u16(bytes, 14)?;
    let compression = read_u32(bytes, 16)?;
    if planes != 1 || width <= 0 || height == 0 {
        return None;
    }
    if !matches!(bit_count, 24 | 32) || !matches!(compression, 0 | 3) {
        return None;
    }
    let (red_mask, green_mask, blue_mask, alpha_mask) = if compression == 3 {
        if header_size < 52 {
            return None;
        }
        (
            read_u32(bytes, 40)?,
            read_u32(bytes, 44)?,
            read_u32(bytes, 48)?,
            if header_size >= 56 {
                read_u32(bytes, 52)?
            } else {
                0
            },
        )
    } else {
        // BI_RGB:24bpp 为 BGR;32bpp 的高字节按规范未定义,不当作 alpha。
        (0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0)
    };
    if red_mask == 0 || green_mask == 0 || blue_mask == 0 {
        return None;
    }
    let width = width as usize;
    let height = height.unsigned_abs() as usize;
    let top_down = height_is_top_down(read_i32(bytes, 8)?);
    let bytes_per_pixel = bit_count as usize / 8;
    let stride = (width * bit_count as usize).div_ceil(32) * 4;
    let data_offset = header_size;
    let needed = data_offset.checked_add(stride.checked_mul(height)?)?;
    if bytes.len() < needed {
        return None;
    }
    let mut rgba = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    for row in 0..height {
        let source_row = data_offset + row * stride;
        let target_row = if top_down { row } else { height - 1 - row };
        for column in 0..width {
            let source = source_row + column * bytes_per_pixel;
            let value = match bit_count {
                24 => u32::from_le_bytes([bytes[source], bytes[source + 1], bytes[source + 2], 0]),
                _ => u32::from_le_bytes([
                    bytes[source],
                    bytes[source + 1],
                    bytes[source + 2],
                    bytes[source + 3],
                ]),
            };
            let target = (target_row * width + column) * 4;
            rgba[target] = mask_channel(value, red_mask);
            rgba[target + 1] = mask_channel(value, green_mask);
            rgba[target + 2] = mask_channel(value, blue_mask);
            rgba[target + 3] = if alpha_mask == 0 {
                255
            } else {
                mask_channel(value, alpha_mask)
            };
        }
    }
    Some(Frame {
        width: width as u32,
        height: height as u32,
        rgba,
        scale: 1.0,
    })
}

#[cfg_attr(not(windows), allow(dead_code))]
fn height_is_top_down(height: i32) -> bool {
    height < 0
}

#[cfg_attr(not(windows), allow(dead_code))]
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[cfg_attr(not(windows), allow(dead_code))]
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

/// 位域通道取 8bit:按 mask 的位移与位宽缩放,标准 24/32bpp 掩码得到原值。
#[cfg_attr(not(windows), allow(dead_code))]
fn mask_channel(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = mask.count_ones();
    let raw = u64::from((value & mask) >> shift);
    let max = if bits >= 32 {
        u64::from(u32::MAX)
    } else {
        (1u64 << bits) - 1
    };
    ((raw * 255 + max / 2) / max) as u8
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

    #[test]
    fn image_prefers_over_text() {
        let image = frame(1, 1, vec![1, 2, 3, 4]);
        let content = classify(Some(image.clone()), Some("hello".into()));
        assert_eq!(content, ClipboardContent::Image(image));
    }

    #[test]
    fn text_content_is_classified_when_non_blank() {
        assert_eq!(
            classify(None, Some("hello\nworld".into())),
            ClipboardContent::Text("hello\nworld".into())
        );
    }

    #[test]
    fn blank_text_is_empty_and_missing_content_is_unsupported() {
        assert_eq!(
            classify(None, Some("  \n\t".into())),
            ClipboardContent::Empty
        );
        assert_eq!(classify(None, None), ClipboardContent::Unsupported);
        // 空图片缓冲不可用时按文本/空继续判定,不当作图片。
        assert_eq!(
            classify(Some(frame(0, 0, Vec::new())), None),
            ClipboardContent::Unsupported
        );
        assert_eq!(
            classify(Some(frame(1, 1, Vec::new())), Some("x".into())),
            ClipboardContent::Text("x".into())
        );
    }

    fn dib_32(width: i32, height: i32, pixels: &[[u8; 4]]) -> Vec<u8> {
        let mut bytes = vec![0u8; 40];
        bytes[0..4].copy_from_slice(&40u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&width.to_le_bytes());
        bytes[8..12].copy_from_slice(&height.to_le_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&32u16.to_le_bytes());
        for pixel in pixels {
            bytes.extend_from_slice(pixel);
        }
        bytes
    }

    #[test]
    fn dib_32_bgr_is_decoded_and_bi_rgb_forces_opaque_alpha() {
        // 底行(先存)蓝、白;顶行(后存)红、绿;BI_RGB 高字节 0 不当透明。
        let dib = dib_32(
            2,
            2,
            &[
                [255, 0, 0, 0],
                [255, 255, 255, 0],
                [0, 0, 255, 0],
                [0, 255, 0, 0],
            ],
        );
        let frame = parse_dib_rgba(&dib).expect("valid DIB");
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(&frame.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&frame.rgba[4..8], &[0, 255, 0, 255]);
        assert_eq!(&frame.rgba[8..12], &[0, 0, 255, 255]);
        assert_eq!(&frame.rgba[12..16], &[255, 255, 255, 255]);
    }

    #[test]
    fn dib_32_top_down_keeps_row_order() {
        // BGRA:顶行(先存)蓝、绿;正高度下同样输入会按底行处理。
        let dib = dib_32(1, -2, &[[255, 0, 0, 0], [0, 255, 0, 0]]);
        let frame = parse_dib_rgba(&dib).expect("valid DIB");
        assert_eq!(&frame.rgba[0..4], &[0, 0, 255, 255]);
        assert_eq!(&frame.rgba[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn dib_24_reads_padded_stride() {
        let mut bytes = vec![0u8; 40];
        bytes[0..4].copy_from_slice(&40u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&2i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&1i32.to_le_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&24u16.to_le_bytes());
        // BGR 红、绿 + 2 字节行填充。
        bytes.extend_from_slice(&[0, 0, 255, 0, 255, 0, 9, 9]);
        let frame = parse_dib_rgba(&bytes).expect("valid 24bpp DIB");
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(&frame.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&frame.rgba[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn dib_rejects_unsupported_and_truncated_headers() {
        assert!(parse_dib_rgba(&[]).is_none());
        assert!(parse_dib_rgba(&[0u8; 39]).is_none());
        let mut sixteen = dib_32(1, 1, &[[0, 0, 0, 0]]);
        sixteen[14..16].copy_from_slice(&16u16.to_le_bytes());
        assert!(parse_dib_rgba(&sixteen).is_none());
        let mut compressed = dib_32(1, 1, &[[0, 0, 0, 0]]);
        compressed[16..20].copy_from_slice(&1u32.to_le_bytes());
        assert!(parse_dib_rgba(&compressed).is_none());
        let mut truncated = dib_32(2, 2, &[[0, 0, 0, 0]; 4]);
        truncated.truncate(48);
        assert!(parse_dib_rgba(&truncated).is_none());
    }

    #[test]
    fn mask_channel_scales_partial_bitfields() {
        assert_eq!(mask_channel(0x00FF_0000, 0x00FF_0000), 255);
        assert_eq!(mask_channel(0, 0x00FF_0000), 0);
        // 5bit 蓝通道全满映射到 255,半值映射到中间值。
        assert_eq!(mask_channel(0x1F, 0x1F), 255);
        assert_eq!(mask_channel(0x10, 0x1F), 132);
    }

    /// 真机 Win32 剪贴板往返:覆盖 PNG/CF_DIBV5、纯文本与只放 CF_DIB 的
    /// 传统应用兜底路径。会覆盖当前剪贴板内容,仅在需要实测时手动运行:
    /// `cargo test --locked --ignored native_clipboard_read_roundtrip`
    #[cfg(windows)]
    #[test]
    #[ignore = "writes the real Windows clipboard; run manually"]
    fn native_clipboard_read_roundtrip() {
        let frame = Frame {
            width: 2,
            height: 2,
            scale: 1.0,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 10, 20, 30, 0],
        };
        crate::clipboard::copy_frame_with_png(
            &frame,
            &crate::capture::buffer::encode_png(&frame).unwrap(),
        )
        .unwrap();
        match read_content().expect("clipboard readable") {
            ClipboardContent::Image(read) => {
                assert_eq!((read.width, read.height), (2, 2));
                assert_eq!(read.rgba, frame.rgba);
            }
            other => panic!("expected an image, got {other:?}"),
        }

        crate::clipboard::copy_text("第一行\n#f00").unwrap();
        assert_eq!(
            read_content().expect("clipboard readable"),
            ClipboardContent::Text("第一行\n#f00".into())
        );

        let dib = dib_32(2, 1, &[[255, 0, 0, 0], [0, 255, 0, 0]]);
        {
            let _clipboard = clipboard_win::Clipboard::new().unwrap();
            clipboard_win::raw::empty().unwrap();
            clipboard_win::raw::set_without_clear(clipboard_win::formats::CF_DIB, &dib).unwrap();
        }
        match read_content().expect("clipboard readable") {
            ClipboardContent::Image(read) => {
                assert_eq!((read.width, read.height), (2, 1));
                assert_eq!(&read.rgba[0..4], &[0, 0, 255, 255]);
                assert_eq!(&read.rgba[4..8], &[0, 255, 0, 255]);
            }
            other => panic!("expected the CF_DIB fallback image, got {other:?}"),
        }
    }
}
