use arboard::Clipboard;
#[cfg(not(windows))]
use arboard::ImageData;

#[cfg(windows)]
mod windows;

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
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    let mut clipboard = Clipboard::new().map_err(|_| {
        CaptureError::api("无法写入剪贴板。")
    })?;
    clipboard
        .set_image(ImageData {
            width: frame.width as usize,
            height: frame.height as usize,
            bytes: std::borrow::Cow::Borrowed(&frame.rgba),
        })
        .map_err(|_| CaptureError::api("无法把截图放入剪贴板。"))
}

pub fn copy_text(text: &str) -> Result<(), CaptureError> {
    let mut clipboard = Clipboard::new().map_err(|_| CaptureError::api("无法写入剪贴板。"))?;
    clipboard
        .set_text(text)
        .map_err(|_| CaptureError::api("无法把文字放入剪贴板。截图仍保留。"))
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
