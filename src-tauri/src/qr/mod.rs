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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrCode {
    /// 解码原文。复制只按这段文本匹配,不使用矩形,也不打开链接。
    pub text: String,
    /// 码身轴对齐外接矩形,帧物理像素。没有可用矩形时宽高为 0。
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrPayload {
    /// 本次识别出的二维码内容,按发现顺序;文本与网址原样返回,不自动打开。
    pub contents: Vec<String>,
    /// 与 `contents` 对齐的外接矩形。复制仍只认文本。
    pub codes: Vec<QrCode>,
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
        Ok(codes) => {
            let contents = codes
                .iter()
                .map(|code| code.text.clone())
                .collect::<Vec<_>>();
            log::info!(
                "qr recognized codes={} size={}x{}",
                contents.len(),
                frame.width,
                frame.height
            );
            runtime.store(Some(contents.clone()));
            Ok(QrPayload { contents, codes })
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
/// 每枚码带上 `Grid::bounds` 的轴对齐外接矩形,供面板避开码身;复制不使用矩形。
pub fn decode_frame(frame: &crate::capture::buffer::Frame) -> Result<Vec<QrCode>, QrError> {
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
    let mut codes = Vec::new();
    let mut grids = prepared.detect_grids();
    for grid in grids.iter_mut() {
        let Ok((_meta, text)) = grid.decode() else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let (x, y, bounds_width, bounds_height) =
            axis_aligned_bounds(grid.bounds, frame.width as i32, frame.height as i32);
        codes.push(QrCode {
            text,
            x,
            y,
            width: bounds_width,
            height: bounds_height,
        });
    }
    if codes.is_empty() {
        Err(QrError::NoCode)
    } else {
        Ok(codes)
    }
}

/// 四角点的轴对齐外接矩形,并钳进帧内。点序是 rqrr 的
/// [左上, 右上, 右下, 左下];宽或高为 0 表示没有可用位置。
fn axis_aligned_bounds(
    bounds: [rqrr::Point; 4],
    frame_w: i32,
    frame_h: i32,
) -> (i32, i32, i32, i32) {
    let min_x = bounds.iter().map(|point| point.x).min().unwrap_or(0);
    let min_y = bounds.iter().map(|point| point.y).min().unwrap_or(0);
    let max_x = bounds.iter().map(|point| point.x).max().unwrap_or(0);
    let max_y = bounds.iter().map(|point| point.y).max().unwrap_or(0);
    let limit_x = frame_w.max(0);
    let limit_y = frame_h.max(0);
    let x0 = min_x.clamp(0, limit_x);
    let y0 = min_y.clamp(0, limit_y);
    let x1 = max_x.clamp(x0, limit_x);
    let y1 = max_y.clamp(y0, limit_y);
    (x0, y0, x1 - x0, y1 - y0)
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
    fn decode_generated_qr_returns_exact_content_and_bounds() {
        let frame = qr_frame("https://example.com/二维码", 4);
        let codes = decode_frame(&frame).expect("sample decodes");
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].text, "https://example.com/二维码");
        assert!(codes[0].width > 0 && codes[0].height > 0);
        assert!(codes[0].x >= 0 && codes[0].y >= 0);
        assert!(codes[0].x + codes[0].width <= frame.width as i32);
        assert!(codes[0].y + codes[0].height <= frame.height as i32);
        // 静区留白,外接矩形应盖住码身中心,而不是整幅白底或贴死边缘。
        let mid = frame.width as i32 / 2;
        let center_x = codes[0].x + codes[0].width / 2;
        let center_y = codes[0].y + codes[0].height / 2;
        assert!((center_x - mid).abs() < frame.width as i32 / 5);
        assert!((center_y - mid).abs() < frame.height as i32 / 5);
        assert!(codes[0].width > frame.width as i32 / 3);
        assert!(codes[0].height > frame.height as i32 / 3);
        assert!(codes[0].width < frame.width as i32);
        assert!(codes[0].height < frame.height as i32);
    }

    #[test]
    fn qr_bounds_are_axis_aligned_and_clamped_to_the_frame() {
        let bounds = [
            rqrr::Point { x: -4, y: 2 },
            rqrr::Point { x: 30, y: -3 },
            rqrr::Point { x: 28, y: 40 },
            rqrr::Point { x: 1, y: 36 },
        ];
        assert_eq!(axis_aligned_bounds(bounds, 32, 32), (0, 0, 30, 32));
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
