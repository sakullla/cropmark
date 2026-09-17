use std::path::{Path, PathBuf};

use image::RgbImage;
use paddle_ocr_rs::ocr_lite::OcrLite;
use tauri::{AppHandle, Manager};

use crate::capture::buffer::Frame;

use super::hit::{aabb, all_indices, expand_for_selection, join_spans, TextSpan};
use super::{OcrDocument, OcrError};

pub const DET_MODEL: &str = "ch_PP-OCRv3_det_infer.onnx";
pub const REC_MODEL: &str = "ch_PP-OCRv3_rec_infer.onnx";
pub const CLS_MODEL: &str = "ch_ppocr_mobile_v2.0_cls_infer.onnx";

const MODEL_FILES: [&str; 3] = [DET_MODEL, REC_MODEL, CLS_MODEL];

pub struct Engine {
    lite: OcrLite,
}

impl Engine {
    pub fn load(dir: &Path) -> Result<Self, OcrError> {
        if !models_present(dir) {
            return Err(OcrError::MissingModels);
        }
        let det = read_model(dir, DET_MODEL)?;
        let cls = read_model(dir, CLS_MODEL)?;
        let rec = read_model(dir, REC_MODEL)?;
        let mut lite = OcrLite::new();
        lite.init_models_from_memory(&det, &cls, &rec, 2)
            .map_err(|_| OcrError::Failed)?;
        Ok(Self { lite })
    }

    pub fn recognize(&mut self, frame: &Frame) -> Result<OcrDocument, OcrError> {
        let rgb = frame_to_rgb(frame)?;
        let result = self
            .lite
            .detect(&rgb, 50, 960, 0.5, 0.3, 1.6, false, false)
            .map_err(|_| OcrError::Failed)?;
        let mut spans = Vec::new();
        for block in result.text_blocks {
            if let Some(span) = span_from_block(&block) {
                spans.push(span);
            }
        }
        let spans = expand_for_selection(&spans);
        if spans.is_empty() {
            return Err(OcrError::NoText);
        }
        let full_text = join_spans(&spans, &all_indices(&spans));
        if full_text.trim().is_empty() {
            return Err(OcrError::NoText);
        }
        Ok(OcrDocument { spans, full_text })
    }
}

pub fn crate_model_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models")
}

pub fn models_present(dir: &Path) -> bool {
    MODEL_FILES.iter().all(|name| dir.join(name).is_file())
}

pub fn resolve_model_dir(app: &AppHandle) -> PathBuf {
    let mut candidates = Vec::new();
    if let Ok(dir) = app.path().resource_dir() {
        candidates.push(dir.join("models"));
        candidates.push(dir);
    }
    if let Ok(dir) = app.path().executable_dir() {
        candidates.push(dir.join("models"));
    }
    candidates.push(crate_model_dir());
    for dir in candidates {
        if models_present(&dir) {
            return dir;
        }
    }
    crate_model_dir()
}

fn read_model(dir: &Path, name: &str) -> Result<Vec<u8>, OcrError> {
    let path = dir.join(name);
    if !path.is_file() {
        return Err(OcrError::MissingModels);
    }
    std::fs::read(&path).map_err(|_| OcrError::MissingModels)
}

fn frame_to_rgb(frame: &Frame) -> Result<RgbImage, OcrError> {
    if frame.width == 0 || frame.height == 0 || frame.rgba.is_empty() {
        return Err(OcrError::Failed);
    }
    let mut rgb = Vec::with_capacity(frame.width as usize * frame.height as usize * 3);
    for px in frame.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    RgbImage::from_raw(frame.width, frame.height, rgb).ok_or(OcrError::Failed)
}

fn span_from_block(block: &paddle_ocr_rs::ocr_result::TextBlock) -> Option<TextSpan> {
    let text = block.text.trim();
    if text.is_empty() {
        return None;
    }
    let points: Vec<(f64, f64)> = block
        .box_points
        .iter()
        .map(|point| (point.x as f64, point.y as f64))
        .collect();
    let (x, y, width, height) = aabb(&points)?;
    Some(TextSpan {
        text: text.to_string(),
        x,
        y,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::{rasterize, Annotation};
    use crate::capture::buffer::{accept_buffer, RawBuffer};

    #[test]
    fn bundled_models_are_local_files_when_present() {
        let dir = crate_model_dir();
        let path = dir.to_string_lossy();
        assert!(!path.contains("://"));
        if models_present(&dir) {
            for name in MODEL_FILES {
                let file = dir.join(name);
                assert!(file.is_file());
                assert!(std::fs::metadata(&file).unwrap().len() > 1024);
            }
        }
    }

    #[test]
    fn recognize_printed_sample_or_skip() {
        let dir = crate_model_dir();
        if !models_present(&dir) {
            return;
        }
        let Ok(mut engine) = Engine::load(&dir) else {
            return;
        };
        let frame = printed_sample();
        match engine.recognize(&frame) {
            Ok(doc) => {
                assert!(!doc.full_text.trim().is_empty());
                let span = &doc.spans[0];
                let hit = crate::ocr::hit::hit_point(
                    &doc.spans,
                    span.x + span.width / 2.0,
                    span.y + span.height / 2.0,
                );
                assert_eq!(hit, Some(0));
            }
            Err(OcrError::NoText) | Err(OcrError::Failed) => {}
            Err(other) => panic!("unexpected ocr error: {other:?}"),
        }
    }

    fn printed_sample() -> Frame {
        let width = 360;
        let height = 72;
        let mut bytes = vec![255u8; (width * height * 4) as usize];
        for px in bytes.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let frame = accept_buffer(RawBuffer::ready(width, height, bytes)).unwrap();
        let ops = vec![Annotation::Text {
            x: 12.0,
            y: 18.0,
            text: "Hello 中文".into(),
            size: 28.0,
            color: crate::annotate::DEFAULT_COLOR.into(),
        }];
        rasterize(&frame, &ops).unwrap_or(frame)
    }
}
