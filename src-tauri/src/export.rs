use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::annotate::{rasterize, Annotation};
use crate::capture::buffer::encode_png;
use crate::capture::error::CaptureError;
use crate::capture::session;
use crate::capture::ui;
use crate::clipboard;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub saved: bool,
}

fn fail(err: CaptureError) -> String {
    let message = err.user_message();
    if message.is_empty() {
        "无法导出当前截图。".into()
    } else {
        message
    }
}

fn annotated_png(app: &AppHandle, annotations: &[Annotation]) -> Result<Vec<u8>, String> {
    let frame = session::current_preview_frame(app).map_err(fail)?;
    let rendered = rasterize(&frame, annotations).map_err(fail)?;
    encode_png(&rendered).map_err(fail)
}

#[tauri::command]
pub fn copy_preview_png(app: AppHandle, annotations: Vec<Annotation>) -> Result<(), String> {
    let frame = session::current_preview_frame(&app).map_err(fail)?;
    let rendered = rasterize(&frame, &annotations).map_err(fail)?;
    clipboard::copy_frame(&rendered).map_err(fail)
}

#[tauri::command]
pub async fn save_preview_png(
    app: AppHandle,
    annotations: Vec<Annotation>,
) -> Result<SaveResult, String> {
    let png = annotated_png(&app, &annotations)?;
    let mut dialog = rfd::AsyncFileDialog::new()
        .add_filter("PNG", &["png"])
        .set_file_name("cropmark.png")
        .set_title("保存截图");
    if let Some(window) = app.get_webview_window(ui::PREVIEW) {
        dialog = dialog.set_parent(&window);
    }
    let Some(file) = dialog.save_file().await else {
        return Ok(SaveResult { saved: false });
    };
    let mut path = file.path().to_path_buf();
    if path.extension().is_none() {
        path.set_extension("png");
    }
    std::fs::write(&path, png).map_err(|_| {
        "无法写入 PNG 文件。预览仍保留，可继续标注、复制或再次保存。".to_string()
    })?;
    session::mark_preview_file_written(&app);
    Ok(SaveResult { saved: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::{exportable, rasterize, Annotation, Point};
    use crate::capture::buffer::{accept_buffer, decode_png, RawBuffer, Frame};

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Frame {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            bytes.extend_from_slice(&color);
        }
        accept_buffer(RawBuffer::ready(width, height, bytes)).unwrap()
    }

    fn checker(width: u32, height: u32) -> Frame {
        let mut bytes = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                if (x + y) % 2 == 0 {
                    bytes[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                } else {
                    bytes[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        }
        accept_buffer(RawBuffer::ready(width, height, bytes)).unwrap()
    }

    #[test]
    fn export_png_rasterizes_rect_pixels() {
        let frame = solid(32, 32, [0, 0, 0, 255]);
        let ops = vec![Annotation::Rect {
            x: 4.0,
            y: 4.0,
            width: 20.0,
            height: 12.0,
        }];
        let rendered = rasterize(&frame, &ops).unwrap();
        let png = encode_png(&rendered).unwrap();
        assert!(png.starts_with(&[137, 80, 78, 71]));
        let decoded = decode_png(&png).unwrap();
        let hit = decoded
            .rgba
            .chunks_exact(4)
            .any(|px| px[0] > 180 && px[1] < 90 && px[2] < 110);
        assert!(hit, "exported PNG should contain the rect stroke");
    }

    #[test]
    fn export_png_rasterizes_mosaic_blocks() {
        let frame = checker(16, 16);
        let ops = vec![Annotation::Mosaic {
            x: 0.0,
            y: 0.0,
            width: 16.0,
            height: 16.0,
            block: 4,
        }];
        let png = encode_png(&rasterize(&frame, &ops).unwrap()).unwrap();
        let decoded = decode_png(&png).unwrap();
        let a = &decoded.rgba[0..4];
        let b = &decoded.rgba[4..8];
        assert_eq!(a, b);
        assert_ne!(&decoded.rgba[0..4], &frame.rgba[0..4]);
    }

    #[test]
    fn empty_text_is_excluded_from_exported_pixels() {
        let frame = solid(24, 24, [12, 24, 36, 255]);
        let empty = vec![Annotation::Text {
            x: 3.0,
            y: 3.0,
            text: "   ".into(),
            size: 18.0,
        }];
        assert!(exportable(&empty).is_empty());
        let rendered = rasterize(&frame, &empty).unwrap();
        assert_eq!(rendered.rgba, frame.rgba);
        let png = encode_png(&rendered).unwrap();
        let decoded = decode_png(&png).unwrap();
        assert_eq!(decoded.rgba, frame.rgba);
    }

    #[test]
    fn confirmed_text_changes_exported_pixels_when_font_exists() {
        let frame = solid(80, 40, [0, 0, 0, 255]);
        let ops = vec![Annotation::Text {
            x: 6.0,
            y: 4.0,
            text: "Hi".into(),
            size: 22.0,
        }];
        if crate::annotate::raster::ui_font().is_none() {
            return;
        }
        let rendered = rasterize(&frame, &ops).unwrap();
        assert_ne!(rendered.rgba, frame.rgba);
    }

    #[test]
    fn arrow_rasterizes_into_frame() {
        let frame = solid(40, 40, [0, 0, 0, 255]);
        let ops = vec![Annotation::Arrow {
            from: Point { x: 4.0, y: 20.0 },
            to: Point { x: 32.0, y: 8.0 },
        }];
        let rendered = rasterize(&frame, &ops).unwrap();
        let hit = rendered
            .rgba
            .chunks_exact(4)
            .any(|px| px[0] > 180 && px[1] < 90);
        assert!(hit);
    }
}
