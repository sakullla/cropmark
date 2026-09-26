mod engine;
pub mod hit;

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::capture::session;
use crate::clipboard;
use crate::i18n;

use engine::{resolve_model_dir, Engine};
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrDocument {
    pub spans: Vec<TextSpan>,
    pub full_text: String,
}

#[derive(Default)]
pub struct OcrRuntime {
    inner: Mutex<OcrInner>,
}

#[derive(Default)]
struct OcrInner {
    engine: Option<Engine>,
    last: Option<OcrDocument>,
}

impl OcrRuntime {
    fn lock(&self) -> std::sync::MutexGuard<'_, OcrInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
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
pub async fn recognize_preview(app: AppHandle) -> Result<OcrDocument, String> {
    let frame =
        session::current_preview_frame(&app).map_err(|_| OcrError::NoPreview.user_message())?;
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || recognize_blocking(&app, &frame))
        .await
        .map_err(|_| OcrError::Failed.user_message())?
}

fn recognize_blocking(
    app: &AppHandle,
    frame: &crate::capture::buffer::Frame,
) -> Result<OcrDocument, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        recognize_blocking_inner(app, frame)
    }))
    .unwrap_or_else(|_| Err(OcrError::Failed.user_message()))
}

fn recognize_blocking_inner(
    app: &AppHandle,
    frame: &crate::capture::buffer::Frame,
) -> Result<OcrDocument, String> {
    // R19:旧 ocrOrientation 开关按常开语义移除,方向纠正保持开启;
    // 引擎按每次识别读取的固定值走同一路径(无需重载模型)。
    let runtime = app.state::<OcrRuntime>();
    let mut inner = runtime.lock();
    if inner.engine.is_none() {
        match Engine::load(&resolve_model_dir(app)) {
            Ok(engine) => inner.engine = Some(engine),
            Err(error) => {
                inner.last = None;
                return Err(error.user_message());
            }
        }
    }
    let engine = inner.engine.as_mut().expect("ocr engine loaded");
    match engine.recognize(frame, true) {
        Ok(doc) => {
            inner.last = Some(doc.clone());
            Ok(doc)
        }
        Err(error) => {
            inner.last = None;
            Err(error.user_message())
        }
    }
}

#[tauri::command]
pub fn copy_ocr_point(app: AppHandle, x: f64, y: f64) -> Result<String, String> {
    let runtime = app.state::<OcrRuntime>();
    let inner = runtime.lock();
    let doc = inner
        .last
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
    let inner = runtime.lock();
    let doc = inner
        .last
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
    let inner = runtime.lock();
    let doc = inner
        .last
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
        let inner = runtime.lock();
        let doc = inner
            .last
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
    let inner = runtime.lock();
    let Some(doc) = inner.last.as_ref() else {
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
