mod engine;
pub mod hit;

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::capture::session;
use crate::clipboard;
use crate::i18n;

use engine::{resolve_model_dir, Engine, OcrStage};
use hit::{
    all_indices, hit_point, hit_rect, join_spans, panel_matches, panel_text_for, PanelMatch, Rect,
    TextSpan,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrError {
    NoText,
    NoSelection,
    Failed,
    MissingModels,
    Copy,
    NoPreview,
}

impl OcrError {
    pub fn key(&self) -> &'static str {
        match self {
            Self::NoText => "error.ocr.no_text",
            Self::NoSelection => "error.ocr.no_selection",
            Self::Failed => "error.ocr.failed",
            Self::MissingModels => "error.ocr.missing_models",
            Self::Copy => "error.ocr.copy",
            Self::NoPreview => "error.ocr.no_preview",
        }
    }

    pub fn user_message(&self) -> String {
        i18n::t(self.key())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrDocument {
    pub spans: Vec<TextSpan>,
    pub full_text: String,
}

/// R6:预览重基后最近一次识别结果的坐标映射:文本、顺序与全文不变,仅 `spans`
/// 按帧变换重映射,保证图上高亮与实际像素位置一致。
pub fn remap_document(
    doc: &OcrDocument,
    transform: crate::annotate::FrameTransform,
) -> OcrDocument {
    OcrDocument {
        spans: doc
            .spans
            .iter()
            .map(|span| {
                let (x, y, width, height) =
                    transform.map_bounds(span.x, span.y, span.width, span.height);
                TextSpan {
                    text: span.text.clone(),
                    x,
                    y,
                    width,
                    height,
                }
            })
            .collect(),
        full_text: doc.full_text.clone(),
    }
}

/// 最近一次识别结果(预览变换快照与重映射共用)。
pub fn last_document(app: &AppHandle) -> Option<OcrDocument> {
    let runtime = app.state::<OcrRuntime>();
    let doc = lock(&runtime.last).doc.clone();
    doc
}

/// 撤销/重做预览变换时恢复同一坐标基准下的识别结果。写入递增代数:
/// 识别期间发生过重基后,进行中的识别结果不再覆盖重基后的文档。
pub fn set_last_document(app: &AppHandle, doc: Option<OcrDocument>) {
    let runtime = app.state::<OcrRuntime>();
    let mut last = lock(&runtime.last);
    last.doc = doc;
    last.revision += 1;
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Default)]
pub struct OcrRuntime {
    /// 推理引擎独立成锁:首次模型加载与整图重试推理只占这把锁,读写
    /// `last` 的命令(预览变换、copy_ocr_*/search)不再被识别阻塞。
    /// 并发识别经内层引擎锁串行,与拆锁前的单锁行为等价。
    engine: Mutex<Option<Arc<Mutex<Engine>>>>,
    /// 最近一次识别结果与写入代数。
    last: Mutex<OcrLast>,
}

#[derive(Default)]
struct OcrLast {
    doc: Option<OcrDocument>,
    revision: u64,
}

pub fn prepared_clipboard_text(text: &str, empty: OcrError) -> Result<&str, OcrError> {
    let text = text.trim();
    if text.is_empty() {
        Err(empty)
    } else {
        Ok(text)
    }
}

pub fn copy_recognized_text(text: &str, empty: OcrError) -> Result<String, OcrError> {
    let text = prepared_clipboard_text(text, empty)?;
    clipboard::copy_text(text).map_err(|_| OcrError::Copy)?;
    Ok(text.to_string())
}

#[tauri::command]
pub async fn recognize_preview(
    app: AppHandle,
    window: tauri::WebviewWindow,
) -> Result<OcrDocument, String> {
    let frame =
        session::current_preview_frame(&app).map_err(|_| OcrError::NoPreview.user_message())?;
    let app = app.clone();
    // R7:阶段事件只发给发起识别的窗口(预览/冻结帧工作区覆盖层共用本命令)。
    let label = window.label().to_string();
    tauri::async_runtime::spawn_blocking(move || recognize_blocking(&app, &label, &frame))
        .await
        .map_err(|_| OcrError::Failed.user_message())?
}

fn recognize_blocking(
    app: &AppHandle,
    label: &str,
    frame: &crate::capture::buffer::Frame,
) -> Result<OcrDocument, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        recognize_blocking_inner(app, label, frame)
    }))
    .unwrap_or_else(|_| Err(OcrError::Failed.user_message()))
}

fn recognize_blocking_inner(
    app: &AppHandle,
    label: &str,
    frame: &crate::capture::buffer::Frame,
) -> Result<OcrDocument, String> {
    // 事件发送失败不阻断识别:前端丢失事件时保持「识别中」常驻兜底。
    let mut emit_stage = |stage: OcrStage| {
        let _ = app.emit_to(label, "ocr-progress", stage);
    };
    // R19:旧 ocrOrientation 开关按常开语义移除,方向纠正保持开启;
    // 引擎按每次识别读取的固定值走同一路径(无需重载模型)。
    // R7:引擎懒加载属 preparing 阶段(首次识别包含模型装载,耗时最长)。
    emit_stage(OcrStage::Preparing);
    let runtime = app.state::<OcrRuntime>();
    // 识别开始时的文档代数:完成后仅当期间无重基写入时才更新最近结果,
    // 旧帧坐标不覆盖重基后的文档(R6:文档与帧同源)。
    let revision = lock(&runtime.last).revision;
    // 引擎槽锁只护住加载本身;随后释放,推理在内层引擎锁上进行。
    let engine = {
        let mut slot = lock(&runtime.engine);
        if slot.is_none() {
            match Engine::load(&resolve_model_dir(app)) {
                Ok(engine) => {
                    log::info!("ocr loaded");
                    *slot = Some(Arc::new(Mutex::new(engine)));
                }
                Err(error) => {
                    log::warn!("ocr load failed kind={}", error.key());
                    store_recognized(&runtime, revision, None);
                    return Err(error.user_message());
                }
            }
        }
        Arc::clone(slot.as_ref().expect("ocr engine loaded"))
    };
    let width = frame.width;
    let height = frame.height;
    let mut engine = lock(&engine);
    let outcome = engine.recognize_reporting(frame, true, &mut emit_stage);
    // 结果写回在引擎锁释放前完成:并发识别后完成者后写入,与拆锁前的
    // 单锁串行顺序一致。
    match outcome {
        Ok(doc) => {
            log::info!(
                "ocr recognized spans={} chars={} size={width}x{height}",
                doc.spans.len(),
                doc.full_text.chars().count()
            );
            store_recognized(&runtime, revision, Some(doc.clone()));
            Ok(doc)
        }
        Err(error) => {
            log::warn!("ocr failed kind={}", error.key());
            store_recognized(&runtime, revision, None);
            Err(error.user_message())
        }
    }
}

/// 识别完成写回最近结果:仅当识别期间没有重基写入(`set_last_document`)
/// 时生效,保证文档坐标始终与当前帧同源。
fn store_recognized(runtime: &OcrRuntime, revision: u64, doc: Option<OcrDocument>) {
    let mut last = lock(&runtime.last);
    if last.revision == revision {
        last.doc = doc;
    }
}

#[tauri::command]
pub fn copy_ocr_point(app: AppHandle, x: f64, y: f64) -> Result<String, String> {
    let runtime = app.state::<OcrRuntime>();
    let inner = lock(&runtime.last);
    let doc = inner
        .doc
        .as_ref()
        .ok_or_else(|| OcrError::NoText.user_message())?;
    let index = hit_point(&doc.spans, x, y).ok_or_else(|| OcrError::NoSelection.user_message())?;
    copy_recognized_text(&doc.spans[index].text, OcrError::NoSelection)
        .map_err(|e| e.user_message())
}

#[tauri::command]
pub fn copy_ocr_rect(
    app: AppHandle,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<String, String> {
    let runtime = app.state::<OcrRuntime>();
    let inner = lock(&runtime.last);
    let doc = inner
        .doc
        .as_ref()
        .ok_or_else(|| OcrError::NoText.user_message())?;
    let hits = hit_rect(
        &doc.spans,
        Rect {
            x,
            y,
            width,
            height,
        },
    );
    let text = join_spans(&doc.spans, &hits);
    copy_recognized_text(&text, OcrError::NoSelection).map_err(|e| e.user_message())
}

#[tauri::command]
pub fn copy_ocr_all(app: AppHandle) -> Result<String, String> {
    let runtime = app.state::<OcrRuntime>();
    let inner = lock(&runtime.last);
    let doc = inner
        .doc
        .as_ref()
        .ok_or_else(|| OcrError::NoText.user_message())?;
    let text = recognized_text(doc);
    copy_recognized_text(&text, OcrError::NoText).map_err(|e| e.user_message())
}

/// 面板复制选中片段或搜索命中的整行。空白、以及不属于本次识别结果的文本都不写入剪贴板。
#[tauri::command]
pub fn copy_ocr_fragment(app: AppHandle, text: String) -> Result<String, String> {
    let fragment = {
        let runtime = app.state::<OcrRuntime>();
        let inner = lock(&runtime.last);
        let doc = inner
            .doc
            .as_ref()
            .ok_or_else(|| OcrError::NoText.user_message())?;
        accepted_fragment(&recognized_text(doc), &text).map_err(|error| error.user_message())?
    };
    copy_recognized_text(&fragment, OcrError::NoSelection).map_err(|error| error.user_message())
}

/// 在最近一次识别全文中搜索。无结果或空查询返回空列表,不改写已保存的识别文本。
#[tauri::command]
pub fn search_ocr_panel(app: AppHandle, query: String) -> Vec<PanelMatch> {
    let runtime = app.state::<OcrRuntime>();
    let inner = lock(&runtime.last);
    let Some(doc) = inner.doc.as_ref() else {
        return Vec::new();
    };
    panel_matches(&recognized_text(doc), &query)
}

fn recognized_text(doc: &OcrDocument) -> String {
    if panel_text_for(&doc.full_text).is_none() {
        join_spans(&doc.spans, &all_indices(&doc.spans))
    } else {
        doc.full_text.clone()
    }
}

/// 片段必须是本次全文的子串;两端空白去掉后为空则拒绝,避免把空内容写入剪贴板。
fn accepted_fragment(source: &str, requested: &str) -> Result<String, OcrError> {
    let source = source.replace("\r\n", "\n").replace('\r', "\n");
    let normalized = requested.replace("\r\n", "\n").replace('\r', "\n");
    let text = normalized.trim();
    if text.is_empty() || !source.contains(text) {
        Err(OcrError::NoSelection)
    } else {
        Ok(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn empty_text_is_rejected_before_clipboard() {
        assert_eq!(
            prepared_clipboard_text("", OcrError::NoText),
            Err(OcrError::NoText)
        );
        assert_eq!(
            prepared_clipboard_text(" \n\t", OcrError::NoSelection),
            Err(OcrError::NoSelection)
        );
        assert_eq!(
            prepared_clipboard_text("中文", OcrError::NoText),
            Ok("中文")
        );
    }

    #[test]
    fn fragment_copy_rejects_blank_and_text_outside_the_result() {
        let source = "左段右段\n下一段";
        assert_eq!(accepted_fragment(source, "右段").as_deref(), Ok("右段"));
        assert_eq!(
            accepted_fragment(source, "左段右段").as_deref(),
            Ok("左段右段")
        );
        assert_eq!(
            accepted_fragment("Hello\r\nWorld", "Hello\nWorld").as_deref(),
            Ok("Hello\nWorld")
        );
        assert_eq!(
            accepted_fragment(source, " \n\t"),
            Err(OcrError::NoSelection)
        );
        assert_eq!(
            accepted_fragment(source, "不存在"),
            Err(OcrError::NoSelection)
        );
        assert_eq!(accepted_fragment(source, ""), Err(OcrError::NoSelection));
    }

    #[test]
    fn recognized_text_uses_full_text_and_falls_back_without_stacking_blank() {
        let joined = OcrDocument {
            spans: vec![TextSpan {
                text: "甲".into(),
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            }],
            full_text: "  ".into(),
        };
        assert_eq!(recognized_text(&joined), "甲");
        let stored = OcrDocument {
            spans: Vec::new(),
            full_text: "甲\n乙".into(),
        };
        assert_eq!(recognized_text(&stored), "甲\n乙");
        assert!(!recognized_text(&stored).contains("甲乙"));
    }

    #[test]
    fn remap_document_moves_spans_with_the_preview_transform() {
        let doc = OcrDocument {
            spans: vec![
                TextSpan {
                    text: "甲".into(),
                    x: 1.0,
                    y: 2.0,
                    width: 3.0,
                    height: 4.0,
                },
                TextSpan {
                    text: "乙".into(),
                    x: 5.0,
                    y: 6.0,
                    width: 2.0,
                    height: 2.0,
                },
            ],
            full_text: "甲乙".into(),
        };
        let rotated = remap_document(
            &doc,
            crate::annotate::FrameTransform::RotateCw { height: 20.0 },
        );
        assert_eq!(rotated.full_text, "甲乙");
        assert_eq!(rotated.spans[0].text, "甲");
        assert_eq!(
            (
                rotated.spans[0].x,
                rotated.spans[0].y,
                rotated.spans[0].width,
                rotated.spans[0].height
            ),
            (20.0 - 2.0 - 4.0, 1.0, 4.0, 3.0)
        );

        let cropped = remap_document(
            &doc,
            crate::annotate::FrameTransform::Crop { dx: -1.0, dy: -2.0 },
        );
        assert_eq!((cropped.spans[0].x, cropped.spans[0].y), (0.0, 0.0));
        assert_eq!(
            (
                cropped.spans[1].x,
                cropped.spans[1].y,
                cropped.spans[1].width,
                cropped.spans[1].height
            ),
            (4.0, 4.0, 2.0, 2.0)
        );
    }

    #[test]
    fn ocr_sources_do_not_open_network() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ocr");
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
        }
    }

    #[test]
    fn product_messages_cover_no_text_and_failure() {
        assert!(OcrError::NoText.user_message().contains("没有识别到文字"));
        assert!(OcrError::Failed.user_message().contains("无法识别"));
        assert!(OcrError::Copy.user_message().contains("截图仍保留"));
    }
}
