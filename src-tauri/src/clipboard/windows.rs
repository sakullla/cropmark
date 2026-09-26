use crate::capture::buffer::{validate_frame, Frame};
use crate::capture::error::CaptureError;

// A positive-height BITMAPV5HEADER plus bottom-up BGRA pixels is accepted by
// legacy Windows applications as well as clients that prefer registered PNG.
fn dib_v5(frame: &Frame) -> Result<Vec<u8>, CaptureError> {
    validate_frame(frame)?;
    if frame.width > i32::MAX as u32
        || frame.height > i32::MAX as u32
        || frame.rgba.len() > u32::MAX as usize
    {
        return Err(CaptureError::invalid_buffer(
            "error.capture.image_too_large",
        ));
    }
    const HEADER: usize = 124;
    let mut dib = vec![0; HEADER + frame.rgba.len()];
    for (offset, value) in [
        (0, HEADER as u32),
        (4, frame.width),
        (8, frame.height),
        (16, 3), // BI_BITFIELDS
        (20, frame.rgba.len() as u32),
        (40, 0x00ff0000),
        (44, 0x0000ff00),
        (48, 0x000000ff),
        (52, 0xff000000),
        (56, 0x73524742), // LCS_sRGB
        (108, 4),         // LCS_GM_IMAGES
    ] {
        dib[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    let stride = frame.width as usize * 4;
    for (src, dst) in frame
        .rgba
        .chunks_exact(stride)
        .rev()
        .zip(dib[HEADER..].chunks_exact_mut(stride))
    {
        for (rgba, bgra) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            bgra.copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
        }
    }
    Ok(dib)
}

pub(super) fn copy_frame_with_png(frame: &Frame, png: &[u8]) -> Result<(), CaptureError> {
    let started = std::time::Instant::now();
    // Prepare both representations before acquiring the global clipboard lock.
    let dib = dib_v5(frame)?;
    let prepared_at = started.elapsed();
    if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        log::warn!("clipboard image failed kind=InvalidBuffer");
        return Err(CaptureError::invalid_buffer("error.capture.png_invalid"));
    }
    let fail = |_error| {
        log::warn!("clipboard write failed kind=image");
        CaptureError::api("error.capture.clipboard_image")
    };
    let format = match clipboard_win::register_format("PNG") {
        Some(format) => format,
        None => {
            log::warn!("clipboard image failed kind=Api");
            return Err(CaptureError::api("error.capture.clipboard_register"));
        }
    };
    // Match arboard/Chromium's bounded retry for other applications temporarily
    // holding the clipboard. Sleep(0) retries exhaust before its owner releases it.
    let mut attempts = 0;
    let _clipboard = loop {
        match clipboard_win::Clipboard::new() {
            Ok(clipboard) => break clipboard,
            Err(error) if attempts == 5 => return Err(fail(error)),
            Err(_) => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    };
    clipboard_win::raw::empty().map_err(fail)?;
    clipboard_win::raw::set_without_clear(format.get(), png).map_err(fail)?;
    clipboard_win::raw::set_without_clear(clipboard_win::formats::CF_DIBV5, &dib).map_err(fail)?;
    log::info!(
        "clipboard image bytes={} size={}x{}",
        png.len(),
        frame.width,
        frame.height
    );
    if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some() {
        eprintln!(
            "Cropmark clipboard: prepare={:?}, write={:?}",
            prepared_at,
            started.elapsed() - prepared_at
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dib_preserves_colors_alpha_and_bottom_up_orientation() {
        let frame = Frame {
            width: 2,
            height: 2,
            scale: 1.5,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 10, 20, 30, 0],
        };
        let dib = dib_v5(&frame).unwrap();
        assert_eq!(dib.len(), 124 + 16);
        for (offset, value) in [
            (0, 124u32),
            (4, 2),
            (8, 2),
            (16, 3),
            (20, 16),
            (40, 0x00ff0000),
            (44, 0x0000ff00),
            (48, 0xff),
            (52, 0xff000000),
            (56, 0x73524742),
        ] {
            assert_eq!(&dib[offset..offset + 4], &value.to_le_bytes());
        }
        assert_eq!(&dib[12..16], &[1, 0, 32, 0]);
        assert_eq!(
            &dib[124..],
            &[255, 0, 0, 64, 30, 20, 10, 0, 0, 0, 255, 255, 0, 255, 0, 128]
        );
    }

    #[test]
    fn malformed_frame_is_rejected_before_touching_clipboard() {
        let frame = Frame {
            width: 2,
            height: 2,
            rgba: vec![0; 4],
            scale: 1.0,
        };
        assert!(copy_frame_with_png(&frame, b"invalid").is_err());
    }

    #[test]
    #[ignore = "writes the real Windows clipboard; run manually"]
    fn native_clipboard_roundtrip() {
        let frame = Frame {
            width: 2,
            height: 2,
            scale: 1.0,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 10, 20, 30, 0],
        };
        let png = crate::capture::buffer::encode_png(&frame).unwrap();
        copy_frame_with_png(&frame, &png).unwrap();
        let mut clipboard = arboard::Clipboard::new().unwrap();
        let image = clipboard.get_image().unwrap();
        assert_eq!((image.width, image.height), (2, 2));
        assert_eq!(image.bytes.as_ref(), frame.rgba);
    }

    #[test]
    #[ignore = "manual DIB conversion benchmark"]
    fn benchmark_dib_conversion() {
        let frame = Frame {
            width: 3840,
            height: 2160,
            scale: 1.0,
            rgba: vec![255; 3840 * 2160 * 4],
        };
        let started = std::time::Instant::now();
        let dib = dib_v5(&frame).unwrap();
        eprintln!(
            "4K DIB conversion: {:?}, {} bytes",
            started.elapsed(),
            dib.len()
        );
    }
}
