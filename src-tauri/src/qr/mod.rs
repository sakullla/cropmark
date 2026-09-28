//! R4:本地二维码识别。纯本地解码(不联网、无运行时资源),只解码二维码,
//! 不含一维条码。识别结果只展示,复制必须由用户显式触发——识别失败或
//! 无结果都不写剪贴板,也不打开任何链接。

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::capture::session;
use crate::clipboard;
use crate::i18n;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrError {
    /// 图里没有二维码,或二维码无法解码出内容。
    NoCode,
    /// 解码过程失败(图像/线程错误),与"没有码"分开说明。
    Failed,
    /// 没有可用的冻结帧/预览帧。
    NoPreview,
    /// 没有可复制的当前结果(结果过期或请求文本不属于本次识别)。
    NoResult,
    /// 写入剪贴板失败。
    Copy,
}

impl QrError {
    pub fn key(&self) -> &'static str {
        match self {
            Self::NoCode => "error.qr.no_code",
            Self::Failed => "error.qr.failed",
            Self::NoPreview => "error.qr.no_preview",
            Self::NoResult => "error.qr.no_result",
            Self::Copy => "error.qr.copy",
        }
    }

    pub fn user_message(&self) -> String {
        i18n::t(self.key())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrPayload {
    /// 本次识别出的二维码内容,按发现顺序;文本与网址原样返回,不自动打开。
    pub contents: Vec<String>,
}

#[derive(Default)]
pub struct QrRuntime {
    inner: Mutex<QrInner>,
}

#[derive(Default)]
struct QrInner {
    last: Option<Vec<String>>,
}

impl QrRuntime {
    fn lock(&self) -> std::sync::MutexGuard<'_, QrInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn store(&self, contents: Option<Vec<String>>) {
        self.lock().last = contents;
    }
}

/// 当前冻结帧(工作区覆盖层)或预览帧的二维码解码入口。帧是同一会话的
/// 冻结画面:选区壳触发时已是裁剪后的区域,预览按钮触发时是预览帧。
#[tauri::command]
pub async fn recognize_qr_preview(app: AppHandle) -> Result<QrPayload, String> {
    let frame =
        session::current_preview_frame(&app).map_err(|_| QrError::NoPreview.user_message())?;
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || recognize_blocking(&app, &frame))
        .await
        .map_err(|_| QrError::Failed.user_message())?
}

fn recognize_blocking(
    app: &AppHandle,
    frame: &crate::capture::buffer::Frame,
) -> Result<QrPayload, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        recognize_blocking_inner(app, frame)
    }))
    .unwrap_or_else(|_| Err(QrError::Failed.user_message()))
}

fn recognize_blocking_inner(
    app: &AppHandle,
    frame: &crate::capture::buffer::Frame,
) -> Result<QrPayload, String> {
    let runtime = app.state::<QrRuntime>();
    match decode_frame(frame) {
        Ok(contents) => {
            log::info!(
                "qr recognized codes={} size={}x{}",
                contents.len(),
                frame.width,
                frame.height
            );
            runtime.store(Some(contents.clone()));
            Ok(QrPayload { contents })
        }
        Err(error) => {
            log::warn!("qr failed kind={}", error.key());
            runtime.store(None);
            Err(error.user_message())
        }
    }
}

/// 面板「复制」:只接受本次识别结果中的原文(不做空白裁剪,保留网址与
/// 大小写)。空串、过期结果与不属于本次识别的文本都不写剪贴板。
#[tauri::command]
pub fn copy_qr_content(app: AppHandle, text: String) -> Result<String, String> {
    let accepted = {
        let runtime = app.state::<QrRuntime>();
        let inner = runtime.lock();
        let contents = inner
            .last
            .as_ref()
            .ok_or_else(|| QrError::NoResult.user_message())?;
        accepted_copy(contents, &text)
            .map_err(|error| error.user_message())?
            .to_string()
    };
    clipboard::copy_text(&accepted).map_err(|_| QrError::Copy.user_message())?;
    Ok(accepted)
}

/// 复制校验(纯逻辑):请求文本必须精确出现在本次识别结果里,空串拒绝。
fn accepted_copy<'a>(contents: &'a [String], requested: &str) -> Result<&'a str, QrError> {
    if requested.is_empty() {
        return Err(QrError::NoResult);
    }
    contents
        .iter()
        .find(|content| content.as_str() == requested)
        .map(String::as_str)
        .ok_or(QrError::NoResult)
}

/// 对整帧做灰度化与二维码检测、解码。检测到码但解码失败不视为整体失败:
/// 只要有一枚可解码就返回;一枚都没有(含无法解码)按「没有二维码」说明。
pub fn decode_frame(frame: &crate::capture::buffer::Frame) -> Result<Vec<String>, QrError> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    if width == 0 || height == 0 || frame.rgba.len() < width * height * 4 {
        return Err(QrError::Failed);
    }
    let rgba = &frame.rgba;
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| {
        let index = (y * width + x) * 4;
        luma(rgba[index], rgba[index + 1], rgba[index + 2])
    });
    let mut contents = Vec::new();
    let mut grids = prepared.detect_grids();
    for grid in grids.iter_mut() {
        let Ok((_meta, text)) = grid.decode() else {
            continue;
        };
        if !text.is_empty() {
            contents.push(text);
        }
    }
    if contents.is_empty() {
        Err(QrError::NoCode)
    } else {
        Ok(contents)
    }
}

/// ITU-R BT.601 灰度:整数近似 0.299R + 0.587G + 0.114B。
fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, Frame, RawBuffer};
    use std::path::PathBuf;

    /// 用纯 Rust 编码器生成样例二维码帧(白底黑码 + 静区 + 放大倍数),
    /// 与产品解码路径共用同一帧结构。
    fn qr_frame(text: &str, scale: u32) -> Frame {
        use qrcode::{Color, QrCode};

        let code = QrCode::new(text.as_bytes()).expect("sample qr encodes");
        let modules = code.width() as u32;
        let quiet = 4u32;
        let size = (modules + quiet * 2) * scale;
        let mut bytes = vec![255u8; (size * size * 4) as usize];
        for px in bytes.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let colors = code.to_colors();
        for my in 0..modules {
            for mx in 0..modules {
                if colors[(my * modules + mx) as usize] != Color::Dark {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = (mx + quiet) * scale + dx;
                        let y = (my + quiet) * scale + dy;
                        let index = ((y * size + x) * 4) as usize;
                        bytes[index..index + 3].copy_from_slice(&[0, 0, 0]);
                    }
                }
            }
        }
        accept_buffer(RawBuffer::ready(size, size, bytes)).expect("sample frame")
    }

    fn blank_frame(size: u32) -> Frame {
        let mut bytes = vec![255u8; (size * size * 4) as usize];
        for px in bytes.chunks_exact_mut(4) {
            px[3] = 255;
        }
        accept_buffer(RawBuffer::ready(size, size, bytes)).expect("blank frame")
    }

    #[test]
    fn decode_generated_qr_returns_exact_content() {
        let frame = qr_frame("https://example.com/二维码", 4);
        let contents = decode_frame(&frame).expect("sample decodes");
        assert_eq!(contents, vec!["https://example.com/二维码".to_string()]);
    }

    #[test]
    fn decode_blank_frame_reports_no_code() {
        assert_eq!(decode_frame(&blank_frame(96)), Err(QrError::NoCode));
    }

    #[test]
    fn decode_degenerate_frame_reports_failure() {
        let frame = Frame {
            width: 0,
            height: 0,
            rgba: Vec::new(),
            scale: 1.0,
        };
        assert_eq!(decode_frame(&frame), Err(QrError::Failed));
    }

    #[test]
    fn copy_accepts_only_current_contents_and_never_blank() {
        let contents = vec!["https://example.com".to_string(), "普通文本".to_string()];
        assert_eq!(
            accepted_copy(&contents, "https://example.com"),
            Ok("https://example.com")
        );
        assert_eq!(accepted_copy(&contents, "普通文本"), Ok("普通文本"));
        assert_eq!(accepted_copy(&contents, ""), Err(QrError::NoResult));
        assert_eq!(accepted_copy(&contents, "不存在"), Err(QrError::NoResult));
        assert_eq!(accepted_copy(&[], "任意"), Err(QrError::NoResult));
    }

    #[test]
    fn luma_matches_bt601_weights() {
        assert_eq!(luma(0, 0, 0), 0);
        assert_eq!(luma(255, 255, 255), 255);
        assert_eq!(luma(255, 0, 0), 76);
        assert_eq!(luma(0, 255, 0), 149);
        assert_eq!(luma(0, 0, 255), 29);
    }

    #[test]
    fn qr_sources_do_not_open_network() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/qr");
        for entry in std::fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let code = src.split("#[cfg(test)]").next().unwrap_or(&src);
            assert!(!code.contains("reqwest"), "{}", path.display());
            assert!(!code.contains("ureq"), "{}", path.display());
            assert!(!code.contains("std::net"), "{}", path.display());
            assert!(!code.contains("TcpStream"), "{}", path.display());
            // 不自动打开链接:解码模块不引用系统打开器。
            assert!(!code.contains("opener"), "{}", path.display());
            assert!(!code.contains("Command::new"), "{}", path.display());
        }
    }

    #[test]
    fn product_messages_cover_no_code_failure_and_copy() {
        assert!(QrError::NoCode.user_message().contains("没有识别到二维码"));
        assert!(QrError::NoResult.user_message().contains("二维码内容"));
        assert!(QrError::Failed.user_message().contains("无法识别"));
        assert!(QrError::Copy.user_message().contains("剪贴板"));
    }
}
