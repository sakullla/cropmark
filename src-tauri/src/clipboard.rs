use arboard::Clipboard;
#[cfg(not(windows))]
use arboard::ImageData;

#[cfg(windows)]
mod windows;

/// R8 剪贴板读取(图片/文本/色块判定),供剪贴板贴图入口使用。
pub mod read;
/// R8 文本/色块贴图的 Rust 排版渲染与原文元数据。
pub mod text_render;

use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;

pub fn copy_frame(frame: &Frame) -> Result<(), CaptureError> {
    #[cfg(windows)]
    {
        let png = crate::capture::buffer::encode_png(frame)?;
        windows::copy_frame_with_png(frame, &png)
    }
    #[cfg(not(windows))]
    copy_frame_native(frame)
}

pub fn copy_frame_with_png(frame: &Frame, png: &[u8]) -> Result<(), CaptureError> {
    #[cfg(windows)]
    {
        windows::copy_frame_with_png(frame, png)
    }
    #[cfg(not(windows))]
    {
        let _ = png;
        copy_frame_native(frame)
    }
}

#[cfg(not(windows))]
fn copy_frame_native(frame: &Frame) -> Result<(), CaptureError> {
    if frame.rgba.is_empty() || frame.width == 0 || frame.height == 0 {
        return Err(CaptureError::invalid_buffer("error.capture.buffer_empty"));
    }
    let mut clipboard =
        Clipboard::new().map_err(|_| CaptureError::api("error.capture.clipboard_write"))?;
    let result = clipboard
        .set_image(ImageData {
            width: frame.width as usize,
            height: frame.height as usize,
            bytes: std::borrow::Cow::Borrowed(&frame.rgba),
        })
        .map_err(|_| CaptureError::api("error.capture.clipboard_image"));
    log_image_result(frame, &result);
    result
}

pub fn copy_text(text: &str) -> Result<(), CaptureError> {
    let bytes = text.len();
    let mut clipboard = match Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(_) => {
            log::warn!("clipboard text failed kind=Api");
            return Err(CaptureError::api("error.capture.clipboard_write"));
        }
    };
    let result = clipboard
        .set_text(text)
        .map_err(|_| CaptureError::api("error.capture.clipboard_text"));
    match &result {
        Ok(()) => log::info!("clipboard text bytes={bytes}"),
        Err(error) => log::warn!("clipboard text failed kind={:?}", error.kind),
    }
    result
}

#[cfg(not(windows))]
pub(crate) fn log_image_result(frame: &Frame, result: &Result<(), CaptureError>) {
    match result {
        Ok(()) => log::info!(
            "clipboard image bytes={} size={}x{}",
            frame.rgba.len(),
            frame.width,
            frame.height
        ),
        Err(error) => log::warn!("clipboard image failed kind={:?}", error.kind),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipboardGuard {
    pub written: bool,
}

impl ClipboardGuard {
    pub fn commit_success(&mut self) {
        self.written = true;
    }

    #[cfg(test)]
    pub fn on_cancel(&self) -> bool {
        !self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_does_not_write_clipboard() {
        let guard = ClipboardGuard::default();
        assert!(guard.on_cancel());
        assert!(!guard.written);
    }

    #[test]
    fn success_marks_clipboard_written() {
        let mut guard = ClipboardGuard::default();
        guard.commit_success();
        assert!(guard.written);
        assert!(!guard.on_cancel());
    }
}
