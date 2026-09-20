use std::borrow::Cow;
use std::path::{Path, PathBuf};

use image::RgbImage;
use paddle_ocr_rs::ocr_lite::OcrLite;
use paddle_ocr_rs::ocr_result::TextBlock;
use tauri::{AppHandle, Manager};

use crate::capture::buffer::Frame;

use super::hit::{aabb, all_indices, expand_for_selection, join_spans, Orientation, TextSpan};
use super::{OcrDocument, OcrError};

pub const DET_MODEL: &str = "ch_PP-OCRv3_det_infer.onnx";
pub const REC_MODEL: &str = "ch_PP-OCRv3_rec_infer.onnx";
pub const CLS_MODEL: &str = "ch_ppocr_mobile_v2.0_cls_infer.onnx";

const MODEL_FILES: [&str; 3] = [DET_MODEL, REC_MODEL, CLS_MODEL];

/// 检测参数沿用原实现:padding=50、max_side_len=960、box_score=0.5、
/// box_thresh=0.3、un_clip_ratio=1.6;`do_angle=true` 用内置角度网逐行做
/// 180° 纠正(R11),`most_angle=false` 保持逐行判定。
const PADDING: u32 = 50;
const MAX_SIDE_LEN: u32 = 960;
const BOX_SCORE_THRESH: f32 = 0.5;
const BOX_THRESH: f32 = 0.3;
const UN_CLIP_RATIO: f32 = 1.6;

/// ADR-9:原图(含逐行角度纠正)结果可信时不重试;无 span 或平均 text_score
/// 低于该值时,再按 90° CW/CCW、180° 旋转整图重试。印刷体正常识别通常 >0.9;
/// 阈值需用真实样例校准(02 遗留 unknown),这里取 0.6 偏保守。
const RETRY_TEXT_SCORE: f32 = 0.6;

pub struct Engine {
    lite: OcrLite,
}

/// 一次旋转候选的识别结果与可信度,用于低置信重试择优(R11)。
#[derive(Debug)]
struct Candidate {
    spans: Vec<TextSpan>,
    quality: Quality,
}

/// 评估口径:span 数优先,平均 text_score 次之;同等取先者。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Quality {
    spans: usize,
    average: f32,
}

impl Quality {
    fn better_than(self, other: Self) -> bool {
        if self.spans != other.spans {
            return self.spans > other.spans;
        }
        self.average > other.average + 1e-6
    }
}

impl Candidate {
    fn from_result(
        result: &paddle_ocr_rs::ocr_result::OcrResult,
        orientation: Orientation,
        frame_width: f64,
        frame_height: f64,
    ) -> Self {
        let mut spans = Vec::with_capacity(result.text_blocks.len());
        let mut score_sum = 0.0_f64;
        for block in &result.text_blocks {
            let Some((span, score)) = span_from_block(block) else {
                continue;
            };
            score_sum += score as f64;
            spans.push(span.mapped_from(orientation, frame_width, frame_height));
        }
        let average = if spans.is_empty() {
            0.0
        } else {
            (score_sum / spans.len() as f64) as f32
        };
        Self {
            quality: Quality {
                spans: spans.len(),
                average,
            },
            spans,
        }
    }

    fn needs_retry(&self) -> bool {
        self.quality.spans == 0 || self.quality.average < RETRY_TEXT_SCORE
    }

    fn better_than(&self, other: &Self) -> bool {
        self.quality.better_than(other.quality)
    }
}

/// ADR-9 候选选择:原图(含逐行角度纠正)可信(有 span 且平均分达阈值)时直接
/// 返回;低置信时按 90° CW、90° CCW、180° 依次重试,与原图候选一起取评估最优
/// (同等取先者)。返回选中候选与实际识别次数,纯逻辑便于单测。
fn select_candidate(
    mut recognize: impl FnMut(Orientation) -> Result<Candidate, OcrError>,
) -> Result<(Candidate, usize), OcrError> {
    let mut orientations = Orientation::RETRY_ORDER.into_iter();
    let mut best = recognize(
        orientations
            .next()
            .expect("retry order starts with identity"),
    )?;
    let mut attempts = 1;
    if !best.needs_retry() {
        return Ok((best, attempts));
    }
    for orientation in orientations {
        let candidate = recognize(orientation)?;
        attempts += 1;
        if candidate.better_than(&best) {
            best = candidate;
        }
    }
    Ok((best, attempts))
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

    /// R24:`orientation_enabled` 为 false 时仅按正置识别:不做逐行 180°
    /// 纠正(`do_angle=false`),也不做整图旋转候选重试。
    pub fn recognize(
        &mut self,
        frame: &Frame,
        orientation_enabled: bool,
    ) -> Result<OcrDocument, OcrError> {
        let rgb = frame_to_rgb(frame)?;
        let frame_width = rgb.width() as f64;
        let frame_height = rgb.height() as f64;

        // R11:先按原图(逐行 180° 纠正)识别;仅当无 span 或平均分过低时,
        // 再按 ADR-9 顺序旋转整图重试,候选坐标逆变换回原图后择优。
        // R24:方向纠正关闭时只识别正置一次。
        let (best, _) = recognize_with_policy(orientation_enabled, |orientation| {
            self.recognize_candidate(
                &rgb,
                orientation,
                orientation_enabled,
                frame_width,
                frame_height,
            )
        })?;
        let spans = expand_for_selection(&best.spans);
        if spans.is_empty() {
            return Err(OcrError::NoText);
        }
        let full_text = join_spans(&spans, &all_indices(&spans));
        if full_text.trim().is_empty() {
            return Err(OcrError::NoText);
        }
        Ok(OcrDocument { spans, full_text })
    }

    /// 单次候选识别:`angle_correction` 控制逐行角度纠正(方向开关关闭时为
    /// false),旋转候选的文本块坐标逆变换回源帧。
    fn recognize_candidate(
        &mut self,
        rgb: &RgbImage,
        orientation: Orientation,
        angle_correction: bool,
        frame_width: f64,
        frame_height: f64,
    ) -> Result<Candidate, OcrError> {
        let rotated = rotate_rgb(rgb, orientation);
        let result = self
            .lite
            .detect(
                &rotated,
                PADDING,
                MAX_SIDE_LEN,
                BOX_SCORE_THRESH,
                BOX_THRESH,
                UN_CLIP_RATIO,
                angle_correction,
                false,
            )
            .map_err(|_| OcrError::Failed)?;
        Ok(Candidate::from_result(
            &result,
            orientation,
            frame_width,
            frame_height,
        ))
    }
}

/// R24:方向纠正策略。开启时沿用候选选择(原图可信即返回,低置信按 ADR-9
/// 顺序整图旋转重试);关闭时只识别正置一次,不重试。
fn recognize_with_policy(
    orientation_enabled: bool,
    mut recognize: impl FnMut(Orientation) -> Result<Candidate, OcrError>,
) -> Result<(Candidate, usize), OcrError> {
    if orientation_enabled {
        select_candidate(recognize)
    } else {
        recognize(Orientation::Identity).map(|candidate| (candidate, 1))
    }
}

/// 与 `Orientation` 的坐标逆变换一一对应的整图旋转(像素映射由 imageops 保证)。
fn rotate_rgb<'a>(rgb: &'a RgbImage, orientation: Orientation) -> Cow<'a, RgbImage> {
    match orientation {
        Orientation::Identity => Cow::Borrowed(rgb),
        Orientation::Rotate90 => Cow::Owned(image::imageops::rotate90(rgb)),
        Orientation::Rotate270 => Cow::Owned(image::imageops::rotate270(rgb)),
        Orientation::Rotate180 => Cow::Owned(image::imageops::rotate180(rgb)),
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

/// 文本块转 AABB span,并带上识别置信度用于旋转候选评估(R11)。
fn span_from_block(block: &TextBlock) -> Option<(TextSpan, f32)> {
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
    Some((
        TextSpan {
            text: text.to_string(),
            x,
            y,
            width,
            height,
        },
        block.text_score,
    ))
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
    fn candidate_quality_prefers_spans_then_score_and_keeps_first_on_tie() {
        let few = Quality {
            spans: 3,
            average: 0.95,
        };
        let many = Quality {
            spans: 4,
            average: 0.20,
        };
        let better = Quality {
            spans: 3,
            average: 0.99,
        };
        assert!(many.better_than(few));
        assert!(better.better_than(few));
        assert!(!few.better_than(better));
        assert!(!better.better_than(Quality {
            spans: 3,
            average: 0.99,
        }));
    }

    /// 合成候选:span 数 + 平均分,便于验证选择与方向策略。
    fn candidate(count: usize, average: f32) -> Candidate {
        Candidate {
            spans: (0..count)
                .map(|index| crate::ocr::hit::TextSpan {
                    text: format!("t{index}"),
                    x: 0.0,
                    y: 0.0,
                    width: 10.0,
                    height: 10.0,
                })
                .collect(),
            quality: Quality {
                spans: count,
                average,
            },
        }
    }

    #[test]
    fn select_candidate_stops_at_first_trustworthy_and_prefers_better_retry() {
        let mut calls = 0;
        let (best, attempts) = select_candidate(|_| {
            calls += 1;
            Ok(candidate(2, 0.95))
        })
        .unwrap();
        assert_eq!(attempts, 1, "可信候选不应触发整图重试");
        assert_eq!(calls, 1);
        assert_eq!(
            best.quality,
            Quality {
                spans: 2,
                average: 0.95
            }
        );

        // 原图无文字 → 90° 有结果 → 继续比对 270°/180°,取 span 更多者。
        let mut order = Vec::new();
        let mut sequence = vec![
            candidate(0, 0.0),
            candidate(1, 0.4),
            candidate(2, 0.5),
            candidate(1, 0.9),
        ]
        .into_iter();
        let (best, attempts) = select_candidate(|orientation| {
            order.push(orientation);
            Ok(sequence.next().unwrap())
        })
        .unwrap();
        assert_eq!(attempts, 4);
        assert_eq!(order, Orientation::RETRY_ORDER.to_vec());
        assert_eq!(
            best.quality,
            Quality {
                spans: 2,
                average: 0.5
            }
        );

        // 全部候选低分:四次重试后保留最优原图候选,不因低分丢弃。
        let mut sequence = vec![
            candidate(1, 0.4),
            candidate(1, 0.3),
            candidate(1, 0.2),
            candidate(1, 0.1),
        ]
        .into_iter();
        let (best, attempts) = select_candidate(|_| Ok(sequence.next().unwrap())).unwrap();
        assert_eq!(attempts, 4);
        assert_eq!(
            best.quality,
            Quality {
                spans: 1,
                average: 0.4
            }
        );

        // 原图低分但后续高分:即使重试结果可信也要与原图比较,取更优者。
        let mut sequence = vec![
            candidate(1, 0.5),
            candidate(1, 0.99),
            candidate(1, 0.98),
            candidate(1, 0.97),
        ]
        .into_iter();
        let (best, attempts) = select_candidate(|_| Ok(sequence.next().unwrap())).unwrap();
        assert_eq!(attempts, 4);
        assert_eq!(
            best.quality,
            Quality {
                spans: 1,
                average: 0.99
            }
        );

        // 识别失败向上传播,不吞掉为 NoText。
        let error = select_candidate(|_| Err(OcrError::Failed)).unwrap_err();
        assert_eq!(error, OcrError::Failed);
    }

    #[test]
    fn orientation_policy_skips_rotation_retries_when_disabled() {
        // 关闭方向纠正:只识别正置一次;即使候选无文字也不旋转重试。
        let mut calls = Vec::new();
        let (best, attempts) = recognize_with_policy(false, |orientation| {
            calls.push(orientation);
            Ok(candidate(0, 0.0))
        })
        .unwrap();
        assert_eq!(calls, vec![Orientation::Identity]);
        assert_eq!(attempts, 1);
        assert_eq!(best.quality.spans, 0);

        // 开启且原图可信:不重试(既有语义不变)。
        let mut calls = Vec::new();
        let (_, attempts) = recognize_with_policy(true, |orientation| {
            calls.push(orientation);
            Ok(candidate(2, 0.95))
        })
        .unwrap();
        assert_eq!(attempts, 1);
        assert_eq!(calls, vec![Orientation::Identity]);

        // 开启且低置信:按 RETRY_ORDER 完整重试。
        let mut calls = Vec::new();
        let (_, attempts) = recognize_with_policy(true, |orientation| {
            calls.push(orientation);
            Ok(candidate(0, 0.0))
        })
        .unwrap();
        assert_eq!(attempts, Orientation::RETRY_ORDER.len());
        assert_eq!(calls, Orientation::RETRY_ORDER.to_vec());

        // 失败照旧向上传播。
        let error = recognize_with_policy(false, |_| Err(OcrError::Failed)).unwrap_err();
        assert_eq!(error, OcrError::Failed);
    }

    #[test]
    fn candidate_maps_rotated_blocks_back_to_source_coordinates() {
        use paddle_ocr_rs::ocr_result::{OcrResult, Point, TextBlock};

        let block = TextBlock {
            box_points: vec![
                Point { x: 10, y: 20 },
                Point { x: 20, y: 20 },
                Point { x: 20, y: 26 },
                Point { x: 10, y: 26 },
            ],
            box_score: 0.9,
            angle_index: 0,
            angle_score: 0.9,
            text: "中文".into(),
            text_score: 0.8,
        };
        let result = OcrResult {
            text_blocks: vec![block],
        };
        let candidate = Candidate::from_result(&result, Orientation::Rotate90, 100.0, 50.0);
        assert_eq!(candidate.quality.spans, 1);
        assert!((candidate.quality.average - 0.8).abs() < 1e-6);
        assert!(!candidate.needs_retry());
        let span = &candidate.spans[0];
        assert_eq!(span.text, "中文");
        assert_eq!(
            (span.x, span.y, span.width, span.height),
            (20.0, 30.0, 6.0, 10.0)
        );

        let blank = TextBlock {
            text: "   ".into(),
            ..result.text_blocks.into_iter().next().unwrap()
        };
        let candidate = Candidate::from_result(
            &OcrResult {
                text_blocks: vec![blank],
            },
            Orientation::Identity,
            100.0,
            50.0,
        );
        assert!(candidate.spans.is_empty());
        assert_eq!(candidate.quality.spans, 0);
        assert!(candidate.needs_retry());
    }

    #[test]
    fn rotated_samples_recognize_with_source_frame_coordinates_or_skip() {
        let dir = crate_model_dir();
        if !models_present(&dir) {
            return;
        }
        let Ok(mut engine) = Engine::load(&dir) else {
            return;
        };
        let upright = printed_sample();
        let Ok(base) = engine.recognize(&upright, true) else {
            return;
        };
        let base_text = base.full_text.trim().to_string();
        assert!(!base_text.is_empty());
        for orientation in [
            Orientation::Rotate180,
            Orientation::Rotate90,
            Orientation::Rotate270,
        ] {
            let frame = rotate_frame(&upright, orientation);
            let doc = engine
                .recognize(&frame, true)
                .unwrap_or_else(|error| panic!("{orientation:?} sample not recognized: {error:?}"));
            assert_eq!(doc.full_text.trim(), base_text, "{orientation:?}");
            for span in &doc.spans {
                assert!(
                    span.x >= -0.5 && span.y >= -0.5,
                    "{orientation:?} span out of frame: {span:?}"
                );
                assert!(
                    span.x + span.width <= frame.width as f64 + 1.0
                        && span.y + span.height <= frame.height as f64 + 1.0,
                    "{orientation:?} span out of frame: {span:?}"
                );
                let (cx, cy) = span.center();
                assert!(
                    crate::ocr::hit::hit_point(&doc.spans, cx, cy).is_some(),
                    "{orientation:?} span center is not selectable: {span:?}"
                );
            }
        }
    }

    #[test]
    fn printed_sample_stays_square_so_rotated_frames_keep_detect_margin() {
        let frame = printed_sample();
        assert_eq!(frame.width, frame.height);
        assert!(
            frame.width >= 240,
            "90°/270° 后短边仍需大于检测 padding,避免窄条误检"
        );
        assert!(!printed_sample_text().is_empty());
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
        match engine.recognize(&frame, true) {
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
        // 正方画布:360×72 横条在 Rotate90/270 后只剩 72px 宽,Linux ONNX 会把
        // 窄条噪声认成额外 "00",与正置结果对不上。两边都给足检测边距。
        let width = 360;
        let height = 360;
        let mut bytes = vec![255u8; (width * height * 4) as usize];
        for px in bytes.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let frame = accept_buffer(RawBuffer::ready(width, height, bytes)).unwrap();
        let ops = vec![Annotation::Text {
            x: 48.0,
            y: 150.0,
            text: printed_sample_text().into(),
            size: 36.0,
            color: crate::annotate::DEFAULT_COLOR.into(),
        }];
        rasterize(&frame, &ops).unwrap_or(frame)
    }

    /// Linux CI 往往没有 CJK 字体;缺字会画出 .notdef 方框,旋转后被认成 "00"。
    fn printed_sample_text() -> &'static str {
        use ab_glyph::Font;
        match crate::annotate::raster::ui_font() {
            Some(font) if font.glyph_id('中').0 != 0 => "Hello 中文",
            _ => "Hello",
        }
    }

    /// 把栅格化样例整体旋转,模拟倒置/侧向截图(R11)。
    fn rotate_frame(frame: &Frame, orientation: Orientation) -> Frame {
        let image = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
            .expect("sample frame is a valid rgba image");
        let rotated = match orientation {
            Orientation::Identity => image,
            Orientation::Rotate90 => image::imageops::rotate90(&image),
            Orientation::Rotate270 => image::imageops::rotate270(&image),
            Orientation::Rotate180 => image::imageops::rotate180(&image),
        };
        let (width, height) = rotated.dimensions();
        Frame {
            width,
            height,
            rgba: rotated.into_raw(),
            scale: frame.scale,
        }
    }
}
