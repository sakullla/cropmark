//! R3 录制保存:复用现有保存对话框骨架、目录记忆与原子写盘。
//!
//! 录制过程只写临时文件;停止后按当前格式弹出保存对话框,确认后把临时文件
//! 复制到目标目录的临时名再改名(跨盘安全),成功后删除临时文件。取消或失败
//! 都保留临时文件,由调用方决定重试或丢弃;失败返回本地化文案,应用继续可用。

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::AppHandle;

use crate::export::{default_capture_file_name, partial_path};
use crate::filename_template::unique_path;
use crate::i18n;
use crate::settings;

use super::{RecordFormat, RecordingOutput};

/// 保存结果:取消时 `saved=false` 且 `path=None`,临时文件保留。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordSaveResult {
    pub saved: bool,
    pub format: RecordFormat,
    pub path: Option<String>,
}

/// 丢弃录制产物(用户取消保存或应用退出清理):删除临时文件。
pub fn discard_recording(output: &RecordingOutput) {
    let _ = std::fs::remove_file(&output.temp_path);
}

/// 用户输入路径 → 目标路径:扩展名与录制格式一致(大小写不敏感)时保留;
/// 缺失扩展名补全当前格式;其余(未知或其它格式的扩展名)追加规范后缀,
/// 避免写出扩展名与内容不符的文件——录制无法在保存时转码。
pub fn resolve_recording_target(path: PathBuf, format: RecordFormat) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case(format.extension()) {
        return path;
    }
    if extension.is_empty() {
        let mut adjusted = path;
        adjusted.set_extension(format.extension());
        return adjusted;
    }
    let mut adjusted = path;
    let name = adjusted
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cropmark".into());
    adjusted.set_file_name(format!("{name}.{}", format.extension()));
    adjusted
}

/// 原子写盘:复制到同目录临时文件再改名;任一步失败都清理临时文件并保留
/// 录制临时文件。成功后删除录制临时文件。
pub fn move_output_atomic(output: &RecordingOutput, path: &Path) -> Result<(), String> {
    let partial = partial_path(path);
    if let Err(error) = std::fs::copy(&output.temp_path, &partial) {
        let _ = std::fs::remove_file(&partial);
        log::warn!("record save failed kind=io");
        return Err(save_error(path, &error));
    }
    if let Err(error) = std::fs::rename(&partial, path) {
        let _ = std::fs::remove_file(&partial);
        log::warn!("record save failed kind=io");
        return Err(save_error(path, &error));
    }
    let _ = std::fs::remove_file(&output.temp_path);
    log::info!(
        "record saved format={} frames={} duration_ms={}",
        output.format.extension(),
        output.frame_count,
        output.duration_ms
    );
    Ok(())
}

fn save_error(path: &Path, error: &std::io::Error) -> String {
    i18n::tp(
        "error.record.save_to_path",
        &[
            ("path", &path.display().to_string()),
            ("error", &error.to_string()),
        ],
    )
}

/// 停止录制后的保存对话框:过滤器与默认文件名按录制格式;起始目录复用
/// 导出目录记忆;成功后更新目录记忆。取消返回 `saved=false`。
pub async fn save_recording_with_dialog(
    app: &AppHandle,
    parent: Option<&tauri::WebviewWindow>,
    output: &RecordingOutput,
) -> Result<RecordSaveResult, String> {
    let export = settings::current_export(app);
    let extension = output.format.extension();
    let mut dialog = rfd::AsyncFileDialog::new()
        .add_filter(i18n::t("dialog.recordings_filter"), &[extension])
        .set_file_name(default_capture_file_name(extension))
        .set_title(i18n::t("dialog.save_recording_title"));
    if let Some(directory) = export.existing_directory() {
        dialog = dialog.set_directory(directory);
    }
    if let Some(window) = parent {
        dialog = dialog.set_parent(window);
    }
    let Some(file) = dialog.save_file().await else {
        return Ok(RecordSaveResult {
            saved: false,
            format: output.format,
            path: None,
        });
    };
    let path = unique_path(&resolve_recording_target(
        file.path().to_path_buf(),
        output.format,
    ));
    move_output_atomic(output, &path)?;
    settings::remember_export_directory(app, path.parent());
    Ok(RecordSaveResult {
        saved: true,
        format: output.format,
        path: Some(path.to_string_lossy().into_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_output(dir: &Path, format: RecordFormat) -> RecordingOutput {
        let temp_path = dir.join(format!("recording.{}", format.extension()));
        fs::write(&temp_path, b"recorded-bytes").expect("temp file");
        RecordingOutput {
            format,
            temp_path,
            width: 320,
            height: 200,
            frame_count: 12,
            duration_ms: 1200,
            auto_stopped: false,
            interrupted: None,
        }
    }

    #[test]
    fn resolve_target_keeps_matching_extensions_and_appends_others() {
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip.gif"), RecordFormat::Gif),
            PathBuf::from("clip.gif")
        );
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip.MP4"), RecordFormat::Mp4),
            PathBuf::from("clip.MP4")
        );
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip"), RecordFormat::Webp),
            PathBuf::from("clip.webp")
        );
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip.mov"), RecordFormat::Mp4),
            PathBuf::from("clip.mov.mp4")
        );
        // 其它录制格式的扩展名不能保留:保存时不转码,内容与扩展名必须一致。
        assert_eq!(
            resolve_recording_target(PathBuf::from("clip.gif"), RecordFormat::Mp4),
            PathBuf::from("clip.gif.mp4")
        );
    }

    #[test]
    fn move_output_atomic_writes_target_and_removes_temp() {
        let dir = std::env::temp_dir().join(format!("cropmark-record-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let output = sample_output(&dir, RecordFormat::Gif);
        let target = dir.join("kept.gif");
        move_output_atomic(&output, &target).expect("save");
        assert_eq!(fs::read(&target).unwrap(), b"recorded-bytes");
        assert!(!output.temp_path.exists(), "temp file must be removed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_output_atomic_reports_localized_error_and_keeps_temp() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-record-save-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let output = sample_output(&dir, RecordFormat::Mp4);
        let unwritable = dir.join("missing-subdir").join("clip.mp4");
        let error = move_output_atomic(&output, &unwritable).unwrap_err();
        assert!(error.contains("无法保存"), "got {error}");
        assert!(error.contains("missing-subdir"), "got {error}");
        assert!(
            output.temp_path.exists(),
            "failed save must keep the recording temp file"
        );
        assert!(!partial_path(&unwritable).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discard_recording_removes_temp_file() {
        let dir = std::env::temp_dir().join(format!("cropmark-record-drop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let output = sample_output(&dir, RecordFormat::Webp);
        discard_recording(&output);
        assert!(!output.temp_path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
