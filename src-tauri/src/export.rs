use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::annotate::{rasterize, Annotation};
use crate::capture::buffer::{encode_jpeg, encode_png, encode_webp, Frame};
use crate::capture::error::CaptureError;
use crate::capture::session;
use crate::capture::ui;
use crate::clipboard;
use crate::i18n;
use crate::settings::{self, ExportFormat, ExportQuality};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveResult {
    pub saved: bool,
    /// 实际写入格式:由用户输入扩展名推导,供前端更新提示与记忆。
    pub format: ExportFormat,
    /// 已写入文件的完整路径;取消时为 None。
    pub path: Option<String>,
}

impl SaveResult {
    fn cancelled(format: ExportFormat) -> Self {
        Self {
            saved: false,
            format,
            path: None,
        }
    }

    pub fn file_name(&self) -> Option<&str> {
        self.path
            .as_deref()
            .and_then(|path| Path::new(path).file_name())
            .and_then(|name| name.to_str())
    }
}

fn fail(err: CaptureError) -> String {
    let message = err.user_message();
    if message.is_empty() {
        i18n::t("error.capture.export")
    } else {
        message
    }
}

fn annotated_frame(app: &AppHandle, annotations: &[Annotation]) -> Result<Frame, String> {
    let frame = session::current_preview_frame(app).map_err(fail)?;
    rasterize(&frame, annotations).map_err(fail)
}

fn encode_for_export(frame: &Frame, format: ExportFormat, quality: u8) -> Result<Vec<u8>, String> {
    let encoded = match format {
        ExportFormat::Png => encode_png(frame),
        ExportFormat::Jpeg => encode_jpeg(frame, quality),
        ExportFormat::Webp => encode_webp(frame, quality),
    };
    encoded.map_err(fail)
}

/// 编码并按目标路径写盘:失败信息包含目标与系统原因,且不触碰预览会话,
/// 保证标注内容与再次保存的机会都保留。
fn write_export(
    path: &Path,
    frame: &Frame,
    format: ExportFormat,
    quality: u8,
) -> Result<(), String> {
    let bytes = encode_for_export(frame, format, quality)?;
    std::fs::write(path, bytes).map_err(|error| {
        i18n::tp(
            "error.capture.save_to_path",
            &[("path", &path.display().to_string()), ("error", &error.to_string())],
        )
    })
}

/// 保存对话框默认文件名:本地时间戳,避免每次都叫 `cropmark.png` 互相覆盖。
/// 形态对齐 Snipaste / ShareX / Flameshot:`Cropmark_2026-09-20_21-45-12.png`。
pub fn default_capture_file_name(extension: &str) -> String {
    format!("Cropmark_{}.{}", local_stamp(), extension)
}

pub fn default_pin_file_name() -> String {
    format!("Cropmark_pin_{}.png", local_stamp())
}

fn local_stamp() -> String {
    let parts = local_date_time();
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        parts[0], parts[1], parts[2], parts[3], parts[4], parts[5]
    )
}

#[cfg(windows)]
fn local_date_time() -> [u32; 6] {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let now = unsafe { GetLocalTime() };
    [
        u32::from(now.wYear),
        u32::from(now.wMonth),
        u32::from(now.wDay),
        u32::from(now.wHour),
        u32::from(now.wMinute),
        u32::from(now.wSecond),
    ]
}

#[cfg(not(windows))]
fn local_date_time() -> [u32; 6] {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut broken = std::mem::zeroed();
        libc::localtime_r(&now, &mut broken);
        [
            (broken.tm_year + 1900) as u32,
            (broken.tm_mon + 1) as u32,
            broken.tm_mday as u32,
            broken.tm_hour as u32,
            broken.tm_min as u32,
            broken.tm_sec as u32,
        ]
    }
}

/// 用户输入路径 → 实际保存路径与格式(R3):已知扩展名以用户输入为准
/// (`jpg`/`jpeg` 都是 JPEG);缺失或未知扩展名回退上次格式并补全规范
/// 后缀,避免写出扩展名与内容不符的文件。
pub fn resolve_target(path: PathBuf, fallback: ExportFormat) -> (PathBuf, ExportFormat) {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    if let Some(format) = ExportFormat::from_extension(extension) {
        return (path, format);
    }
    if extension.is_empty() {
        let mut adjusted = path;
        adjusted.set_extension(fallback.extension());
        return (adjusted, fallback);
    }
    let mut adjusted = path;
    let name = adjusted
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cropmark".into());
    adjusted.set_file_name(format!("{name}.{}", fallback.extension()));
    (adjusted, fallback)
}

#[tauri::command]
pub fn copy_preview_png(app: AppHandle, annotations: Vec<Annotation>) -> Result<(), String> {
    let frame = session::current_preview_frame(&app).map_err(fail)?;
    let rendered = rasterize(&frame, &annotations).map_err(fail)?;
    clipboard::copy_frame(&rendered).map_err(fail)
}

/// 预览保存:目标格式由保存对话框返回的扩展名推导,缺失/未知回退设置的
/// 上次格式;成功后写入格式、目录与质量档位记忆。写盘失败返回明确错误,
/// 预览会话与标注不回滚。
#[tauri::command]
pub async fn save_preview_png(
    app: AppHandle,
    annotations: Vec<Annotation>,
    quality: Option<ExportQuality>,
) -> Result<SaveResult, String> {
    let frame = annotated_frame(&app, &annotations)?;
    let mut export = settings::current_export(&app);
    if let Some(quality) = quality {
        export.quality = quality;
    }
    let parent = app.get_webview_window(ui::PREVIEW);
    save_frame_with_dialog(&app, frame, export, parent.as_ref()).await
}

/// 预览保存与静默保存共用的对话框与写盘流程;静默路径无父窗口(parent=None)。
pub async fn save_frame_with_dialog(
    app: &AppHandle,
    frame: Frame,
    export: settings::ExportSettings,
    parent: Option<&tauri::WebviewWindow>,
) -> Result<SaveResult, String> {
    let fallback = export.last_format;
    let mut dialog = rfd::AsyncFileDialog::new()
        .add_filter(i18n::t("dialog.images_filter"), &["png", "jpg", "jpeg", "webp"])
        .set_file_name(default_capture_file_name(fallback.extension()))
        .set_title(i18n::t("dialog.save_capture_title"));
    if let Some(directory) = export.existing_directory() {
        dialog = dialog.set_directory(directory);
    }
    if let Some(window) = parent {
        dialog = dialog.set_parent(window);
    }
    let Some(file) = dialog.save_file().await else {
        return Ok(SaveResult::cancelled(fallback));
    };
    let (path, format) = resolve_target(file.path().to_path_buf(), fallback);
    write_export(&path, &frame, format, export.quality.value())?;
    session::mark_preview_file_written(app);
    settings::remember_export(app, format, export.quality, path.parent());
    Ok(SaveResult {
        saved: true,
        format,
        path: Some(path.to_string_lossy().into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::{exportable, rasterize, Annotation, Point};
    use crate::capture::buffer::{accept_buffer, decode_png, Frame, RawBuffer};

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
            color: crate::annotate::DEFAULT_COLOR.into(),
            stroke_width: None,
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
            color: crate::annotate::DEFAULT_COLOR.into(),
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
            color: crate::annotate::DEFAULT_COLOR.into(),
        }];
        if crate::annotate::raster::ui_font().is_none() {
            return;
        }
        let rendered = rasterize(&frame, &ops).unwrap();
        assert_ne!(rendered.rgba, frame.rgba);
    }

    #[test]
    fn default_capture_file_name_uses_local_timestamp() {
        let name = default_capture_file_name("png");
        assert!(name.starts_with("Cropmark_"), "{name}");
        assert!(name.ends_with(".png"), "{name}");
        let stamp = name
            .strip_prefix("Cropmark_")
            .and_then(|rest| rest.strip_suffix(".png"))
            .expect("stamp");
        let (date, time) = stamp.split_once('_').expect("date_time");
        assert_eq!(date.len(), 10, "{date}");
        assert_eq!(time.len(), 8, "{time}");
        assert_eq!(date.chars().filter(|ch| *ch == '-').count(), 2);
        assert_eq!(time.chars().filter(|ch| *ch == '-').count(), 2);
        let pin = default_pin_file_name();
        assert!(pin.starts_with("Cropmark_pin_"));
        assert!(pin.ends_with(".png"));
    }

    #[test]
    fn resolve_target_keeps_known_extensions_and_derives_format() {
        let (path, format) = resolve_target(PathBuf::from("shot.png"), ExportFormat::Jpeg);
        assert_eq!(path, PathBuf::from("shot.png"));
        assert_eq!(format, ExportFormat::Png);

        let (path, format) = resolve_target(PathBuf::from("shot.JPG"), ExportFormat::Png);
        assert_eq!(path, PathBuf::from("shot.JPG"));
        assert_eq!(format, ExportFormat::Jpeg);

        let (path, format) = resolve_target(PathBuf::from("shot.jpeg"), ExportFormat::Png);
        assert_eq!(path, PathBuf::from("shot.jpeg"));
        assert_eq!(format, ExportFormat::Jpeg);

        let (path, format) = resolve_target(PathBuf::from("shot.webp"), ExportFormat::Png);
        assert_eq!(path, PathBuf::from("shot.webp"));
        assert_eq!(format, ExportFormat::Webp);
    }

    #[test]
    fn resolve_target_appends_fallback_extension_when_missing() {
        let (path, format) = resolve_target(PathBuf::from("D:/shots/cropmark"), ExportFormat::Png);
        assert_eq!(path, PathBuf::from("D:/shots/cropmark.png"));
        assert_eq!(format, ExportFormat::Png);

        let (path, format) = resolve_target(PathBuf::from("cropmark"), ExportFormat::Jpeg);
        assert_eq!(path, PathBuf::from("cropmark.jpg"));
        assert_eq!(format, ExportFormat::Jpeg);

        let (path, format) = resolve_target(PathBuf::from("cropmark"), ExportFormat::Webp);
        assert_eq!(path, PathBuf::from("cropmark.webp"));
        assert_eq!(format, ExportFormat::Webp);
    }

    #[test]
    fn resolve_target_keeps_unknown_extension_and_appends_canonical_suffix() {
        let (path, format) = resolve_target(PathBuf::from("shot.v2"), ExportFormat::Jpeg);
        assert_eq!(path, PathBuf::from("shot.v2.jpg"));
        assert_eq!(format, ExportFormat::Jpeg);

        let (path, format) = resolve_target(PathBuf::from("shot."), ExportFormat::Png);
        assert_eq!(path, PathBuf::from("shot.png"));
        assert_eq!(format, ExportFormat::Png);
    }

    #[test]
    fn export_branches_encode_expected_containers_for_each_format() {
        let frame = solid(24, 16, [200, 40, 60, 0]);
        let png = encode_for_export(&frame, ExportFormat::Png, 90).unwrap();
        assert!(png.starts_with(&[137, 80, 78, 71]));
        let jpeg = encode_for_export(&frame, ExportFormat::Jpeg, 90).unwrap();
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        let webp = encode_for_export(&frame, ExportFormat::Webp, 90).unwrap();
        assert!(webp.starts_with(b"RIFF") && &webp[8..12] == b"WEBP");
        assert!(!png.is_empty() && !jpeg.is_empty() && !webp.is_empty());
    }

    #[test]
    fn write_export_writes_decodable_files_for_each_format_and_reports_failure() {
        let dir = std::env::temp_dir().join(format!("cropmark-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let frame = solid(32, 24, [30, 120, 200, 255]);

        let png_path = dir.join("shot.png");
        write_export(&png_path, &frame, ExportFormat::Png, 90).unwrap();
        let png = std::fs::read(&png_path).unwrap();
        assert!(png.starts_with(&[137, 80, 78, 71]));
        assert_eq!(decode_png(&png).unwrap().rgba, frame.rgba);

        let jpeg_path = dir.join("shot.jpg");
        write_export(&jpeg_path, &frame, ExportFormat::Jpeg, 80).unwrap();
        let jpeg = std::fs::read(&jpeg_path).unwrap();
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        assert!(image::load_from_memory(&jpeg).is_ok());

        let webp_path = dir.join("shot.webp");
        write_export(&webp_path, &frame, ExportFormat::Webp, 80).unwrap();
        let webp = std::fs::read(&webp_path).unwrap();
        assert!(webp.starts_with(b"RIFF") && &webp[8..12] == b"WEBP");
        assert!(webp::Decoder::new(&webp).decode().is_some());

        let unwritable = dir.join("missing-subdir").join("shot.png");
        let error = write_export(&unwritable, &frame, ExportFormat::Png, 90).unwrap_err();
        assert!(error.contains("无法保存到"), "got {error}");
        assert!(error.contains("预览仍保留"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn arrow_rasterizes_into_frame() {
        let frame = solid(40, 40, [0, 0, 0, 255]);
        let ops = vec![Annotation::Arrow {
            from: Point { x: 4.0, y: 20.0 },
            to: Point { x: 32.0, y: 8.0 },
            color: crate::annotate::DEFAULT_COLOR.into(),
            stroke_width: None,
        }];
        let rendered = rasterize(&frame, &ops).unwrap();
        let hit = rendered
            .rgba
            .chunks_exact(4)
            .any(|px| px[0] > 180 && px[1] < 90);
        assert!(hit);
    }
}
